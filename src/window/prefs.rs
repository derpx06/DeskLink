//! The Preferences page, for the selected phone and this computer: Access,
//! Connection and Appearance, and Unpair. With no phone paired yet the
//! computer-wide part is offered as a dialog instead.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::ui_kit::{self, ButtonKind, Tone};
use super::{clipboard, DesklinkWindow};
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::relay_link::RELAY_ENABLED;

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
            "{:?}{:?}{}{}{:?}",
            glide_status.availability,
            glide_status.problem,
            glide_status.asking,
            glide_status.can_ask_again,
            glide_status.ready
        ),
        // The phone's "allowed" report changes the glide row's words.
        crate::device_links::glide::phone_allows(&device.id),
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
    let titles: Vec<String> = groups
        .iter()
        .map(|group| group.title().to_string())
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

/// Every settings row inside `widget`.
fn rows_in(widget: &gtk::Widget, found: &mut Vec<adw::PreferencesRow>) {
    if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
        found.push(row.clone());
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        rows_in(&next, found);
        child = next.next_sibling();
    }
}

/// The widget name of a row whose visibility is its own state (the glide
/// arrangement follows the glide switch): the search leaves it alone.
const OWN_VISIBILITY: &str = "own-visibility";

/// A markup string as the plain text it shows ("Privacy &amp; security" is
/// "Privacy & security"); text that is not markup is itself.
fn plain(markup: &str) -> String {
    gtk::pango::parse_markup(markup, '\0')
        .map_or_else(|_| markup.to_string(), |(_, text, _)| text.to_string())
}

/// A group as the search sees it: the group, its rows, and which of them
/// were shown before any search (a row hidden on purpose stays hidden).
struct Searchable {
    group: adw::PreferencesGroup,
    rows: Vec<(adw::PreferencesRow, bool)>,
    shown: bool,
}

fn searchable(groups: &[adw::PreferencesGroup]) -> Vec<Searchable> {
    groups
        .iter()
        .map(|group| {
            let mut rows = Vec::new();
            rows_in(group.upcast_ref(), &mut rows);
            Searchable {
                group: group.clone(),
                rows: rows
                    .into_iter()
                    .filter(|row| row.widget_name() != OWN_VISIBILITY)
                    .map(|row| {
                        let shown = row.is_visible();
                        (row, shown)
                    })
                    .collect(),
                shown: group.is_visible(),
            }
        })
        .collect()
}

/// Shows only the rows (and groups) whose title or words contain `query`;
/// a group whose own title matches shows whole. True when anything shows.
fn filter_groups(groups: &[Searchable], query: &str) -> bool {
    // Typed text against the plain words of the rows, not their markup.
    let query = query.trim().to_lowercase();
    let mut any = false;
    for Searchable { group, rows, shown } in groups {
        let whole = query.is_empty() || plain(&group.title()).to_lowercase().contains(&query);
        let mut matching = 0;
        for (row, was_shown) in rows {
            let mut words = plain(&row.title()).to_lowercase();
            if let Some(subtitle) = row
                .downcast_ref::<adw::ActionRow>()
                .and_then(|action| action.subtitle())
            {
                words.push(' ');
                words.push_str(&plain(&subtitle).to_lowercase());
            }
            let matches = whole || words.contains(&query);
            row.set_visible(*was_shown && matches);
            matching += usize::from(*was_shown && matches);
        }
        let visible = *shown && (matching > 0 || rows.is_empty());
        group.set_visible(visible);
        any |= visible;
    }
    any
}

/// The groups of the Settings page, in the order the phone uses too:
/// Connection, Notifications, Clipboard, Calls, Sound, Control, Files,
/// Privacy & security, Appearance, Advanced. Only what people change is
/// here; everything else is a fixed default.
pub(super) const GROUPS: [&str; 10] = [
    "Connection",
    "Notifications",
    "Clipboard",
    "Calls",
    "Sound",
    "Control",
    "Files",
    "Privacy & security",
    "Appearance",
    "Advanced",
];

/// Text for a row, group or description that reads markup: escaped, so a
/// device named `Pixel & <9>` reads as typed.
fn esc(text: impl AsRef<str>) -> glib::GString {
    glib::markup_escape_text(text.as_ref())
}

fn group(title: &str) -> adw::PreferencesGroup {
    // A group's title is read as markup: "Privacy & security" is not valid
    // markup as it is, and the heading was not drawn at all.
    adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(title))
        .build()
}

fn groups(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    glide_status: &crate::device_links::glide::Status,
    approving: bool,
) -> Vec<adw::PreferencesGroup> {
    let connection = group(GROUPS[0]);
    connection.add(&keep_awake_row(daemon, settings));
    connection.add(&phone_keep_awake_row(daemon, settings, device));
    connection.add(&start_at_login_row(daemon, settings));

    let notifications = group(GROUPS[1]);
    notifications.set_description(Some(
        "Where phone notifications show, and which of this computer's go to the phone",
    ));
    let shown = show_notifications_row(daemon, settings, device);
    notifications.add(&shown);
    notifications.add(&command_alerts_row(daemon, settings, device));
    add_forward_rows(&notifications, daemon, settings);
    notifications.add(&dnd_row(daemon, settings, device));

    let clipboard = group(GROUPS[2]);
    clipboard.add(&clipboard_row(daemon, settings));

    let calls = calls_group(daemon, settings, device);

    let sound = sound_group(daemon, settings);

    let control = group(GROUPS[5]);
    add_glide_rows(&control, daemon, settings, device, glide_status);
    control.add(&remote_control_row(device, approving));

    let files = group(GROUPS[6]);
    files.add(&bucket_row(device));
    files.add(&downloads_row());
    files.add(&gallery_row(daemon, settings, device));

    let privacy = group(GROUPS[7]);
    privacy.add(&password_row(daemon, settings));
    privacy.add(&terminal_access_row(daemon, device));
    privacy.add(&unlock_nearby_row(daemon, settings, device));
    privacy.add(&walk_away_row(daemon, settings, device));
    privacy.add(&unpair_row(daemon, device));

    let appearance = group(GROUPS[8]);
    appearance.add(&appearance_row());
    add_top_bar_rows(&appearance, daemon, settings);

    let advanced = group(GROUPS[9]);
    if let Some(row) = relay_status_row(RELAY_ENABLED) {
        advanced.add(&row);
    }
    advanced.add(&history_row());
    advanced.add(&system_check_row());
    advanced.add(&logs_row());

    // The group titles are fixed (see GROUPS); calls and sound build theirs.
    calls.set_title(&glib::markup_escape_text(GROUPS[3]));
    sound.set_title(&glib::markup_escape_text(GROUPS[4]));
    vec![
        connection,
        notifications,
        clipboard,
        calls,
        sound,
        control,
        files,
        privacy,
        appearance,
        advanced,
    ]
}

/// "Show <phone>'s notifications here": in its group and among the quick switches.
fn show_notifications_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let shown = adw::SwitchRow::builder()
        .title(esc(format!("Show {}'s notifications here", device.name)))
        .subtitle("On this desktop, with replies")
        .active(settings.shows_notifications(&device.id))
        .build();
    with_icon(&shown, "preferences-system-notifications-symbolic");
    let (sender, id) = (daemon.clone(), device.id.clone());
    toggled(&shown, move |row| {
        sender.send(DaemonCommand::SetShowNotifications(
            id.clone(),
            row.is_active(),
        ));
    });
    follow(&shown, |settings, id| settings.shows_notifications(id));
    shown
}

/// The computer-wide settings as a dialog, for when no phone is paired.
pub(crate) fn show_dialog(parent: &impl IsA<gtk::Widget>, daemon: &DaemonHandle) {
    let settings = crate::device_links::config::app_settings();
    let dialog = adw::PreferencesDialog::builder()
        .title("Settings")
        .content_width(640)
        .build();
    let page = adw::PreferencesPage::new();
    let connection = group(GROUPS[0]);
    connection.add(&keep_awake_row(daemon, &settings));
    connection.add(&start_at_login_row(daemon, &settings));
    page.add(&connection);
    let privacy = group(GROUPS[7]);
    privacy.add(&password_row(daemon, &settings));
    page.add(&privacy);
    let appearance = group(GROUPS[8]);
    appearance.add(&appearance_row());
    add_top_bar_rows(&appearance, daemon, &settings);
    page.add(&appearance);
    let advanced = group(GROUPS[9]);
    if let Some(row) = relay_status_row(RELAY_ENABLED) {
        advanced.add(&row);
    }
    advanced.add(&history_row());
    advanced.add(&system_check_row());
    advanced.add(&logs_row());
    page.add(&advanced);
    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Sound (F1 A3): whether a phone may hear this computer's microphone.
fn sound_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Sound").build();
    let mic = adw::SwitchRow::builder()
        .title("Phones may hear this computer's microphone")
        .subtitle("Only when one asks; a coral bar shows while it listens")
        .active(settings.desk_mic_to_phone)
        .build();
    with_icon(&mic, "audio-input-microphone-symbolic");
    let sender = daemon.clone();
    mic.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetDeskMicToPhone(row.is_active()))
    });
    // No PipeWire (or its GStreamer plugin): off, with the reason.
    if let Some(reason) = crate::platform::reason("sound") {
        mic.set_sensitive(false);
        mic.set_subtitle(&esc(reason));
        mic.set_subtitle_lines(0);
    }
    group.add(&mic);
    group
}

/// Notifications to Phone (C1): this computer's notifications on the
/// phones, off by default and only from the apps ticked here (the apps seen
/// since DeskLink started, and those already chosen).
fn add_forward_rows(group: &adw::PreferencesGroup, daemon: &DaemonHandle, settings: &AppSettings) {
    if let Some(reason) = crate::device_links::desk_notifications::unavailable() {
        let row = adw::ActionRow::builder()
            .title("Send this computer's notifications to the phone")
            .subtitle(esc(reason))
            .build();
        with_icon(&row, "preferences-system-notifications-symbolic");
        group.add(&row);
        return;
    }
    let on = adw::SwitchRow::builder()
        .title("Send this computer's notifications to the phone")
        .subtitle("Only from the apps you choose. The phone's own are never sent back.")
        .active(settings.forward_notifications)
        .build();
    with_icon(&on, "preferences-system-notifications-symbolic");
    let sender = daemon.clone();
    on.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetForwardNotifications(row.is_active()))
    });
    group.add(&on);
    if !settings.forward_notifications {
        return;
    }
    let mut apps = crate::device_links::desk_notifications::seen_apps();
    for key in &settings.forward_apps {
        apps.entry(key.clone()).or_insert_with(|| key.clone());
    }
    if apps.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No notifications yet")
            .subtitle("Apps appear here once they show a notification")
            .build();
        group.add(&row);
    }
    for (key, name) in apps {
        let row = adw::SwitchRow::builder()
            .title(&name)
            .subtitle(if name == key { "" } else { key.as_str() })
            .use_markup(false)
            .active(settings.forward_apps.contains(&key))
            .build();
        let sender = daemon.clone();
        row.connect_active_notify(move |row| {
            sender.send(DaemonCommand::SetForwardApp(key.clone(), row.is_active()))
        });
        group.add(&row);
    }
}

/// Calls: answered here; callers can be named from the phone's contacts.
fn calls_group(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Calls")
        .description(glib::markup_escape_text(&format!(
            "Answer, decline and end {}'s calls here. Turn on Calls in DeskLink on the phone.",
            device.name
        )))
        .build();
    group.add(&pause_music_row(daemon, settings));
    // F5 K4: names from the contacts the phone shares, off until asked.
    let names = adw::SwitchRow::builder()
        .title(esc(format!("Name callers from {}'s contacts", device.name)))
        .subtitle("Uses the contacts the phone shares with this computer")
        .active(settings.names_callers_from(&device.id))
        .build();
    with_icon(&names, "avatar-default-symbolic");
    let (sender, id) = (daemon.clone(), device.id.clone());
    names.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetContactCallerNames(
            id.clone(),
            row.is_active(),
        ))
    });
    group.add(&names);
    group
}

/// "Pause music during calls": this computer's players pause while a call
/// rings or runs on the phone, and play again after.
fn pause_music_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Pause music during calls")
        .subtitle(COMPUTER_WIDE_MUSIC_SUBTITLE)
        .active(settings.pause_media_on_call)
        .build();
    with_icon(&row, "media-playback-pause-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetPauseMediaOnCall(row.is_active()))
    });
    follow(&row, |settings, _| settings.pause_media_on_call);
    row
}

/// "Share the clipboard": the same switch as the Notes & Clipboard header.
/// A computer-wide switch on a per-phone page says so (J18): it was read as
/// belonging to the phone whose page it sits on.
const COMPUTER_WIDE_CLIPBOARD_TITLE: &str = "Share this computer's clipboard with phones";
const COMPUTER_WIDE_CLIPBOARD_SUBTITLE: &str =
    "Applies to every paired phone. Copy on one device, paste on the other. Passwords are never shared.";
const COMPUTER_WIDE_MUSIC_SUBTITLE: &str =
    "Applies to every paired phone. This computer's players pause while a call rings or runs, and carry on after";

fn clipboard_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(COMPUTER_WIDE_CLIPBOARD_TITLE)
        .subtitle(COMPUTER_WIDE_CLIPBOARD_SUBTITLE)
        .active(settings.clipboard_sync)
        .build();
    with_icon(&row, "edit-paste-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetClipboardSync(row.is_active()))
    });
    follow(&row, |settings, _| settings.clipboard_sync);
    row
}

/// "The Bucket": on or off for this phone.
fn bucket_row(device: &DeviceView) -> adw::SwitchRow {
    let on = crate::device_links::folders::bucket_on(&device.id);
    let row = adw::SwitchRow::builder()
        .title(esc(format!("Bucket with {}", device.name)))
        .subtitle("One place for files, pictures, text and links on both devices")
        .active(on)
        .build();
    with_icon(&row, "folder-symbolic");
    let id = device.id.clone();
    row.connect_active_notify(move |row| {
        if row.is_active() {
            crate::device_links::folders::turn_on_bucket(&id);
        } else if crate::device_links::folders::bucket_on(&id) {
            crate::device_links::folders::remove(&id, crate::device_links::folders::BUCKET_ID);
        }
    });
    row
}

/// Where received files land, one click away.
fn downloads_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Downloads folder")
        .subtitle("Files from the phone land here; a name already taken becomes \"name (2)\"")
        .activatable(true)
        .build();
    with_icon(&row, "folder-download-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let Some(dir) = glib::user_special_dir(glib::UserDirectory::Downloads) else {
            return;
        };
        gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
            row.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            |result| {
                if let Err(error) = result {
                    eprintln!("[UI] Could not open Downloads: {error}");
                }
            },
        );
    });
    row
}

/// "Connection history": the log of connections and sessions, in a dialog.
fn history_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Connection history")
        .subtitle("Every connection and session from a phone, kept for 30 days")
        .activatable(true)
        .build();
    with_icon(&row, "document-open-recent-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let types = row
            .root()
            .and_downcast::<DesklinkWindow>()
            .and_then(|window| window.daemon().map(|daemon| (window, daemon)))
            .map(|(window, daemon)| {
                window
                    .known_devices(&daemon)
                    .into_iter()
                    .map(|known| (known.id, known.device_type))
                    .collect()
            })
            .unwrap_or_default();
        super::history::show_dialog(row, &types, None);
    });
    row
}

/// "Logs": where crash reports are kept.
fn system_check_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("System check")
        .subtitle("What works on this computer, and what to install for the rest")
        .activatable(true)
        .build();
    with_icon(&row, "preferences-system-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(super::system_check::show_dialog);
    row
}

fn logs_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Crash reports")
        .subtitle("Opens the folder DeskLink writes a report to if it ever crashes")
        .activatable(true)
        .build();
    with_icon(&row, "text-x-generic-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let Some(dir) = crate::crash_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let (shown, widget) = (dir.display().to_string(), row.clone());
        gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
            row.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            move |result| {
                if let Err(error) = result {
                    eprintln!("[UI] Could not open the crash folder: {error}");
                    // No file manager (a bare window manager) is not a silent no-op.
                    super::toast_from(
                        &widget,
                        &format!("No file manager opened it. The reports are in {shown}"),
                    );
                }
            },
        );
    });
    row
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

fn keep_awake_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Keep this computer awake while a phone is connected")
        .subtitle("Lid closed included, only while a paired phone is connected; it sleeps again when the phone goes away. Applies to every phone.")
        .active(settings.stay_awake)
        .build();
    with_icon(&row, "weather-clear-night-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetStayAwake(row.is_active()))
    });
    follow(&row, |settings, _| settings.stay_awake);
    row
}

/// Unlock with the phone: off unless turned on here, and explained.
fn unlock_nearby_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "When this computer is locked, {phone} can unlock it after you approve on the \
         phone — only on the same network{}. Other phones cannot.",
        if RELAY_ENABLED {
            ", never through the relay"
        } else {
            ""
        }
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Unlock with {phone} nearby"))
        .use_markup(false)
        .active(settings.unlocks_from(&device.id))
        .build();
    // Set after `use_markup(false)`: a subtitle given to the builder is read
    // as markup first, and a phone named `A & B` was a warning.
    row.set_subtitle(&explained);
    with_icon(&row, "changes-allow-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetUnlockNearby(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.unlocks_from(id));
    row
}

/// Photos and screenshots the phone sends on its own (W12): off unless turned
/// on here (the phone has its own switch too).
fn gallery_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "New screenshots and photos {phone} sends on its own appear on the Files page. They are \
         kept here for 30 days, up to 200 MB, and never in Downloads. {phone} has its own switch too."
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Accept photos and screenshots from {phone}"))
        .use_markup(false)
        .active(settings.accepts_gallery_from(&device.id))
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "image-x-generic-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetGalleryAccept(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.accepts_gallery_from(id));
    row
}

/// Do Not Disturb shared with the phone (W14): off unless turned on here.
fn dnd_row(daemon: &DaemonHandle, settings: &AppSettings, device: &DeviceView) -> adw::SwitchRow {
    let phone = &device.name;
    let supported = crate::device_links::dnd::desk_state().is_some();
    let mut explained = if supported {
        format!(
            "{phone}'s Do Not Disturb turns off banners here, and turning them off here asks \
             {phone} to do the same. Put back when you stop sharing or {phone} is gone for a minute."
        )
    } else {
        "Not available on this desktop (it needs GNOME's notification settings)".to_string()
    };
    if supported && settings.shares_dnd_with(&device.id) && device.dnd_can_set == Some(false) {
        explained.push_str(&format!(
            " {phone} can't be controlled from here until you allow Do Not Disturb access on it."
        ));
    }
    let row = adw::SwitchRow::builder()
        .title(format!("Share Do Not Disturb with {phone}"))
        .use_markup(false)
        .active(settings.shares_dnd_with(&device.id))
        .sensitive(supported)
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "notifications-disabled-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetDndShare(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.shares_dnd_with(id));
    row
}

/// Walk-away lock: off unless turned on here, and explained.
fn walk_away_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "Locks this computer if nothing reaches {phone} for a minute (three if its screen \
         was off), after a ten second warning you can cancel. Not while this computer has no \
         network, and it never unlocks."
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Lock when I walk away with {phone}"))
        .use_markup(false)
        .active(settings.locks_when_away(&device.id))
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "system-lock-screen-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetWalkAwayLock(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.locks_when_away(id));
    row
}

fn command_alerts_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Command alerts")
        .use_markup(false)
        .active(settings.alerts_commands_to(&device.id))
        .build();
    row.set_subtitle(&format!(
        "Tell {} when a terminal command that ran over 10 seconds finishes",
        device.name
    ));
    with_icon(&row, "alarm-symbolic");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetCommandAlerts(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.alerts_commands_to(id));
    row
}

/// Whether this phone may switch keep-awake from its side: off, with the
/// reason in the subtitle.
fn phone_keep_awake_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Let this phone keep the computer awake")
        .use_markup(false)
        .active(settings.lets_keep_awake(&device.id))
        .build();
    row.set_subtitle(&format!(
        "{} may turn \"keep awake\" on, lid closed included. A laptop kept awake in a bag gets hot and flattens its battery.",
        device.name
    ));
    with_icon(&row, "weather-clear-night-symbolic");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetKeepAwakeFrom(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.lets_keep_awake(id));
    row
}

fn start_at_login_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Start at Login")
        .subtitle(crate::platform::note("autostart").map_or_else(
            || glib::GString::from("Keeps this computer reachable after a restart"),
            esc,
        ))
        .subtitle_lines(0)
        .active(settings.start_at_login.unwrap_or(true))
        .build();
    with_icon(&row, "system-shutdown-symbolic");
    let daemon = daemon.clone();
    row.connect_active_notify(move |row| {
        daemon.send(DaemonCommand::SetStartAtLogin(row.is_active()))
    });
    row
}

/// The extension's own top-bar item, besides its Quick Settings entry.
fn top_bar_pill_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Phone in the top bar")
        .subtitle("Also show the phone as its own item, not only in Quick Settings")
        .active(settings.top_bar_pill)
        .build();
    with_icon(&row, "phone-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetTopBarPill(row.is_active()))
    });
    follow(&row, |settings, _| settings.top_bar_pill);
    row
}

fn tray_hosted() -> bool {
    gio::Application::default()
        .and_downcast::<crate::application::DesklinkApplication>()
        .is_some_and(|app| app.tray_hosted())
}

/// What the Top Bar group shows, to redraw it when that changes.
fn top_bar_key() -> String {
    format!(
        "{} {:?} {:?}",
        tray_hosted(),
        crate::shell_dbus::extension_status(),
        crate::shell_dbus::appindicator_off()
    )
}

/// "Log Out Now…" (GNOME asks to confirm).
fn log_out_button() -> gtk::Button {
    let button = gtk::Button::builder()
        .label("Log Out Now…")
        .valign(gtk::Align::Center)
        .build();
    button.connect_clicked(|_| crate::shell_dbus::log_out());
    button
}

/// Top Bar: whether DeskLink shows there, and the one step that fixes it.
/// The tray icon is the freedesktop way (KDE, most desktops; GNOME with the
/// AppIndicator extension); GNOME also has DeskLink's own extension.
fn add_top_bar_rows(group: &adw::PreferencesGroup, daemon: &DaemonHandle, settings: &AppSettings) {
    use crate::shell_dbus::ExtensionStatus;
    let gnome = crate::shell_dbus::extension_possible();

    let tray = adw::ActionRow::builder().title("Tray icon").build();
    with_icon(&tray, "phone-symbolic");
    let indicator_off = crate::shell_dbus::appindicator_off();
    tray.set_subtitle(if tray_hosted() {
        "Shown"
    } else if indicator_off.is_some() {
        "Not shown — the AppIndicator extension is installed but off"
    } else if gnome {
        "Not available — GNOME shows tray icons only with the AppIndicator extension"
    } else {
        "Not available on this desktop"
    });
    if let Some(uuid) = indicator_off.filter(|_| !tray_hosted()) {
        let button = gtk::Button::builder()
            .label("Turn on")
            .valign(gtk::Align::Center)
            .build();
        button.update_property(&[gtk::accessible::Property::Label(
            "Turn on the AppIndicator extension",
        )]);
        button.connect_clicked(move |button| {
            button.set_sensitive(false);
            let weak = button.downgrade();
            crate::shell_dbus::enable_extension(&uuid, move |result| {
                let Some(button) = weak.upgrade() else { return };
                button.set_sensitive(true);
                if let Err(error) = result {
                    super::toast_from(&button, &format!("Could not turn it on: {error}"));
                }
            });
        });
        tray.add_suffix(&button);
    }
    group.add(&tray);

    if !gnome {
        return;
    }
    let menu = adw::ActionRow::builder().title("GNOME extension").build();
    with_icon(&menu, "view-app-grid-symbolic");
    match crate::shell_dbus::extension_status() {
        ExtensionStatus::Active => {
            menu.set_subtitle(
                "Active — your phone, live sessions and quick actions in the top bar",
            );
            let off = gtk::Button::builder()
                .label("Turn off")
                .valign(gtk::Align::Center)
                .build();
            off.update_property(&[gtk::accessible::Property::Label(
                "Turn off the top-bar menu",
            )]);
            off.connect_clicked(|button| match crate::shell_dbus::turn_off_extension() {
                Ok(()) => {
                    button.set_sensitive(false);
                    super::toast_from(button, "Top-bar menu turned off");
                }
                Err(error) => super::toast_from(button, &format!("Could not turn it off: {error}")),
            });
            menu.add_suffix(&off);
        }
        ExtensionStatus::NeedsLogin => {
            menu.set_subtitle("Installed — it appears after you log in again");
            menu.add_suffix(&log_out_button());
        }
        ExtensionStatus::Off => {
            menu.set_subtitle(
                "Off — your phone, live sessions and quick actions in the top bar, and Unlock on the lock screen. Turning it on also allows GNOME user extensions (a setting for all of them); Turn Off disables only this one.",
            );
            let button = gtk::Button::builder()
                .label("Turn on")
                .valign(gtk::Align::Center)
                .build();
            button.update_property(&[gtk::accessible::Property::Label("Turn on the top-bar menu")]);
            button.connect_clicked(|button| match crate::shell_dbus::turn_on_extension() {
                Ok(true) => {
                    let dialog = adw::AlertDialog::new(
                        Some("Top-Bar Menu Installed"),
                        Some(crate::shell_dbus::RELOGIN_NOTE),
                    );
                    dialog.add_response("later", "_Later");
                    dialog.add_response("log-out", "_Log Out Now…");
                    dialog.set_close_response("later");
                    dialog.connect_response(Some("log-out"), |_, _| crate::shell_dbus::log_out());
                    dialog.present(Some(button));
                }
                Ok(false) => super::toast_from(button, "Top-bar menu turned on"),
                Err(error) => {
                    eprintln!("[UI] Could not turn on the top-bar menu: {error}");
                    let dialog = adw::AlertDialog::new(
                        Some("Could Not Turn On the Top-Bar Menu"),
                        Some(&error),
                    );
                    dialog.add_response("ok", "OK");
                    dialog.present(Some(button));
                }
            });
            menu.add_suffix(&button);
        }
    }
    group.add(&menu);
    group.add(&top_bar_pill_row(daemon, settings));
}

/// System, Light or Dark.
fn appearance_row() -> adw::ComboRow {
    use crate::application::{appearance_scheme, load_appearance, save_appearance};
    let choices = ["system", "light", "dark"];
    let row = adw::ComboRow::builder()
        .title("Style")
        .model(&gtk::StringList::new(&["System", "Light", "Dark"]))
        .selected(
            choices
                .iter()
                .position(|choice| *choice == load_appearance())
                .unwrap_or(0) as u32,
        )
        .build();
    row.add_prefix(&icon_square(
        "preferences-desktop-appearance-symbolic",
        30,
        "row-icon",
    ));
    row.connect_selected_notify(move |row| {
        let choice = choices
            .get(row.selected() as usize)
            .copied()
            .unwrap_or("system");
        save_appearance(choice);
        adw::StyleManager::default().set_color_scheme(appearance_scheme(choice));
    });
    row
}

fn terminal_access_row(daemon: &DaemonHandle, device: &DeviceView) -> adw::SwitchRow {
    let warning = format!("Lets {} open terminals on this computer", device.name);
    // `active` is set before the handler is connected, so building the row
    // from persisted state does not itself send a command.
    let row = adw::SwitchRow::builder()
        .title("Terminal access")
        .use_markup(false)
        .active(device.terminal_access)
        .build();
    row.set_subtitle(&warning);
    with_icon(&row, "utilities-terminal-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&warning)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    row.connect_active_notify(move |row| {
        debug_log!("[UI] Terminal access for {id} set to {}", row.is_active());
        daemon.send(DaemonCommand::SetTerminalAccess(
            id.clone(),
            row.is_active(),
        ));
    });
    row
}

/// With the relay compiled in (`relay_enabled`, i.e. `RELAY_ENABLED`): one
/// read-only line, where the relay is and how it is doing. Never a switch,
/// an address field or a TURN choice: the constant is the only control, and
/// the address lives in config.json. `None` with the relay compiled out.
fn relay_status_row(relay_enabled: bool) -> Option<adw::ActionRow> {
    if !relay_enabled {
        return None;
    }
    let row = adw::ActionRow::builder()
        .title("Away from home")
        .use_markup(false)
        .activatable(false)
        .build();
    row.add_prefix(&icon_square("desklink-globe-symbolic", 30, "row-icon"));
    let update = |row: &adw::ActionRow| {
        use crate::device_links::relay_link::client::{status, RelayStatus};
        let settings = crate::device_links::config::app_settings();
        let state = match status() {
            _ if !settings.relay_configured() => "no relay server built in".to_string(),
            RelayStatus::Off => "waiting for a paired phone".to_string(),
            RelayStatus::Connecting => "connecting…".to_string(),
            RelayStatus::Online => "ready · end-to-end encrypted".to_string(),
            RelayStatus::Unavailable(reason) => format!("unavailable: {reason}"),
            RelayStatus::UpdateRequired => "update DeskLink to connect away from home".to_string(),
        };
        row.set_subtitle(&match relay_host(settings.relay_url()) {
            "" => state,
            host => format!("{host} · {state}"),
        });
    };
    update(&row);
    let weak = row.downgrade();
    crate::ticker::every_tick(move || match weak.upgrade() {
        Some(row) => {
            update(&row);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    Some(row)
}

/// "relay.desklink.app" from "wss://relay.desklink.app/v1".
fn relay_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

fn password_subtitle(set: bool) -> &'static str {
    if set {
        "Phones give this computer's session password once per connection"
    } else {
        "Pairing alone is enough"
    }
}

/// "Ask before terminal and screen access": the session password, on or
/// off, and Change… while it is on.
fn password_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::ActionRow {
    let set = settings.session_password.is_some();
    let row = adw::ActionRow::builder()
        .title("Ask before terminal and screen access")
        .subtitle(password_subtitle(set))
        .build();
    row.add_prefix(&icon_square("auth-fingerprint-symbolic", 30, "row-icon"));
    let change = gtk::Button::builder()
        .label("Change…")
        .valign(gtk::Align::Center)
        .visible(set)
        .css_classes(["flat"])
        .build();
    let switch = gtk::Switch::builder()
        .active(set)
        .valign(gtk::Align::Center)
        .build();
    switch.update_property(&[gtk::accessible::Property::Label(
        "Ask for a password before terminal and screen access",
    )]);
    row.add_suffix(&change);
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    // Weak references: the controls live inside the row they update.
    let (shown_row, shown_change) = (row.downgrade(), change.downgrade());
    let show = move |set: bool| {
        if let Some(row) = shown_row.upgrade() {
            row.set_subtitle(password_subtitle(set));
        }
        if let Some(change) = shown_change.upgrade() {
            change.set_visible(set);
        }
    };
    let show = std::rc::Rc::new(show);
    {
        let (daemon, show) = (daemon.clone(), show.clone());
        change.connect_clicked(move |button| {
            let (daemon, show) = (daemon.clone(), show.clone());
            ask_for_password(button, move |password| {
                if let Some(password) = password {
                    daemon.send(DaemonCommand::SetSessionPassword(Some(password)));
                    show(true);
                }
            });
        });
    }
    let daemon = daemon.clone();
    switch.connect_state_set(move |switch, on| {
        if !on {
            // Also reached when a cancelled dialog puts the switch back.
            if crate::device_links::config::app_settings()
                .session_password
                .is_some()
            {
                daemon.send(DaemonCommand::SetSessionPassword(None));
            }
            show(false);
            return glib::Propagation::Proceed;
        }
        // On only once a password is chosen; cancelling leaves it off.
        let (daemon, show, shown) = (daemon.clone(), show.clone(), switch.downgrade());
        ask_for_password(switch, move |password| {
            let Some(switch) = shown.upgrade() else {
                return;
            };
            match password {
                Some(password) => {
                    daemon.send(DaemonCommand::SetSessionPassword(Some(password)));
                    switch.set_state(true);
                    show(true);
                }
                None => {
                    switch.set_state(false);
                    switch.set_active(false);
                }
            }
        });
        glib::Propagation::Stop
    });
    row
}

/// Asks for a new session password twice; `done` gets it when both match,
/// or `None` when the dialog is cancelled.
fn ask_for_password(parent: &impl IsA<gtk::Widget>, done: impl Fn(Option<String>) + 'static) {
    let entry = adw::PasswordEntryRow::builder()
        .title("New password")
        .build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Confirm password")
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    list.append(&confirm);

    let dialog = adw::AlertDialog::new(
        Some("Session password"),
        Some("Your phone asks for this once per connection before it can view or control the screen or open a terminal."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Set password")]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("ok", false);
    dialog.set_extra_child(Some(&list));

    // Weak: `check` is connected on the entries and enables the dialog.
    let check = {
        let dialog = dialog.downgrade();
        let entry = entry.downgrade();
        let confirm = confirm.downgrade();
        move || {
            let (Some(dialog), Some(entry), Some(confirm)) =
                (dialog.upgrade(), entry.upgrade(), confirm.upgrade())
            else {
                return;
            };
            let password = entry.text();
            let usable = !password.is_empty()
                && password.len() <= crate::device_links::session_lock::MAX_PASSWORD_BYTES
                && password == confirm.text();
            dialog.set_response_enabled("ok", usable);
        }
    };
    entry.connect_changed({
        let check = check.clone();
        move |_| check()
    });
    confirm.connect_changed(move |_| check());

    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        done((response == "ok").then(|| entry.text().to_string()));
    });
}

/// Approve, while at the desk, the phone controlling this computer — so
/// later sessions from the phone start without anyone here to click Share.
pub(super) fn approve_remote_control(window: &DesklinkWindow, id: &str) {
    const VIEW_ONLY: &str =
        "Shared without remote interaction, so not remembered. Try again with \u{201c}Allow Remote Interaction\u{201d} on.";
    window.imp().approving.borrow_mut().insert(id.to_string());
    window.refresh_soon();
    let (window, id) = (window.downgrade(), id.to_string());
    glib::spawn_future_local(async move {
        let worker_id = id.clone();
        let outcome = gio::spawn_blocking(move || {
            crate::device_links::webrtc::preauthorize_remote_control(&worker_id)
        })
        .await
        .unwrap_or_else(|_| Err("The approval was interrupted".to_string()));
        let Some(window) = window.upgrade() else {
            return;
        };
        window.imp().approving.borrow_mut().remove(&id);
        match &outcome {
            Ok(true) => window.show_feature_toast("Approved"),
            Ok(false) => window.show_feature_toast(VIEW_ONLY),
            Err(error) => {
                eprintln!("[UI] Remote-control approval did not complete: {error}");
                window.show_feature_toast("The approval did not complete");
            }
        }
        window.refresh_soon();
    });
}

/// "Let the phone control mouse and keyboard": approve it here once, so the
/// phone's later sessions need no one at the desk.
///
/// Drawn from state only (the remembered grant and whether an approval is
/// under way), because the page is redrawn when either changes and a label
/// set by hand would be lost.
fn remote_control_row(device: &DeviceView, approving: bool) -> adw::ActionRow {
    const APPROVED: &str = "Approved until DeskLink restarts";
    const NOT_APPROVED: &str = "The desktop asks you to approve each time";
    const WAITING: &str =
        "Choose the screen, turn on \u{201c}Allow Remote Interaction\u{201d}, then Share";

    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    let row = adw::ActionRow::builder()
        .title("Let the phone control mouse and keyboard")
        .subtitle(match (approving, approved) {
            (true, _) => WAITING,
            (false, true) => APPROVED,
            (false, false) => NOT_APPROVED,
        })
        .build();
    row.add_prefix(&icon_square("input-mouse-symbolic", 30, "row-icon"));
    let approve = gtk::Button::builder()
        .label(match (approving, approved) {
            (true, _) => "Choose the Screen…",
            (false, true) => "Approve Again…",
            (false, false) => "Approve…",
        })
        .sensitive(!approving)
        .valign(gtk::Align::Center)
        .build();
    let forget = gtk::Button::builder()
        .label("Forget")
        .valign(gtk::Align::Center)
        .visible(approved && !approving)
        .css_classes(["flat"])
        .build();
    forget.update_property(&[gtk::accessible::Property::Description(
        "Stop this phone controlling the computer without asking",
    )]);
    row.add_suffix(&forget);
    row.add_suffix(&approve);
    // A desktop that cannot share its screen says why, instead of a button
    // that opens nothing.
    if let Some(reason) = crate::platform::reason("share") {
        approve.set_sensitive(false);
        row.set_subtitle(&esc(reason));
        row.set_subtitle_lines(0);
    }
    let id = device.id.clone();
    approve.connect_clicked(move |button| {
        if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
            approve_remote_control(&window, &id);
        }
    });
    let id = device.id.clone();
    forget.connect_clicked(move |button| {
        crate::device_links::config::set_remote_desktop_token(&id, None);
        if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
            window.refresh_soon();
        }
    });
    row
}

/// "Unpair Pixel 9…", red, after asking.
fn unpair_row(daemon: &DaemonHandle, device: &DeviceView) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(esc(format!("Unpair {}…", device.name)))
        .subtitle("It can connect again only after pairing again")
        .activatable(true)
        .css_classes(["error"])
        .build();
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    row.connect_activated(move |row| {
        let (daemon, id) = (daemon.clone(), id.clone());
        super::confirm(
            row,
            &format!("Unpair {name}?"),
            "It can connect again only after pairing again.",
            "_Unpair",
            move || {
                debug_log!("[UI] Unpair clicked for {}", id);
                daemon.send(DaemonCommand::Unpair(id.clone()));
            },
        );
    });
    row
}

/// The words under the glide switch: what it does, or (until the phone says
/// it allows glide) what to do on the phone first. Glide stays off until then.
fn glide_explained(phone: &str, allowed: Option<bool>, computer: &str) -> String {
    if allowed == Some(true) {
        format!(
            "Move the pointer past the edge of this screen to use {phone} with this mouse and \
             keyboard. Esc+Super brings it back. GNOME asks you to allow this once each time \
             DeskLink starts and again after the screen locks; it cannot remember the answer."
        )
    } else {
        format!("Turn on “Let {computer} control this phone” on the phone first.")
    }
}

/// Glide (F4): the pointer glides onto this phone past an edge of this
/// screen. A switch, and where the phone sits (a small arrangement: this
/// screen with a phone slot on each side). Hidden, with the reason, where
/// the desktop cannot capture input.
fn add_glide_rows(
    group: &adw::PreferencesGroup,
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    status: &crate::device_links::glide::Status,
) {
    use crate::device_links::glide::{Availability, Edge};
    match status.availability {
        Availability::Checking => return,
        Availability::Unavailable(reason) => {
            let row = adw::ActionRow::builder()
                .title(esc(format!("Glide onto {}", device.name)))
                .subtitle(esc(reason))
                .build();
            with_icon(&row, "input-mouse-symbolic");
            row.update_property(&[gtk::accessible::Property::Description(reason)]);
            group.add(&row);
            return;
        }
        Availability::Available => {}
    }
    let phone = device.name.clone();
    let current = settings
        .glide
        .as_ref()
        .filter(|glide| glide.device == device.id)
        .map(|glide| glide.edge);
    let explained = match status.ready.filter(|edge| current == Some(*edge)) {
        Some(edge) => format!(
            "Ready: move the pointer off the {} edge of this screen to use {phone}. Esc+Super \
             brings it back.",
            match edge {
                Edge::Left => "left",
                Edge::Right => "right",
                Edge::Top => "top",
                Edge::Bottom => "bottom",
            }
        ),
        None => glide_explained(
            &phone,
            crate::device_links::glide::phone_allows(&device.id),
            &crate::device_links::glide::desk_name(),
        ),
    };
    let switch = adw::SwitchRow::builder()
        .title(esc(format!("Glide onto {phone}")))
        .subtitle(esc(&explained))
        .active(current.is_some())
        .build();
    with_icon(&switch, "input-mouse-symbolic");
    switch.update_property(&[gtk::accessible::Property::Description(&explained)]);
    follow(&switch, |settings, id| {
        settings
            .glide
            .as_ref()
            .is_some_and(|glide| glide.device == id)
    });
    group.add(&switch);

    // The arrangement: a phone slot on each side of this screen.
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(8)
        .halign(gtk::Align::Center)
        .margin_top(14)
        .margin_bottom(14)
        .build();
    let screen = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(132)
        .height_request(80)
        .valign(gtk::Align::Center)
        .css_classes(["glide-screen"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    let monitor = gtk::Image::builder()
        .icon_name("desklink-device-desktop-symbolic")
        .pixel_size(24)
        .vexpand(true)
        .valign(gtk::Align::End)
        .build();
    screen.append(&monitor);
    let label = gtk::Label::builder()
        .label("This screen")
        .vexpand(true)
        .valign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();
    screen.append(&label);
    grid.attach(&screen, 1, 1, 1, 1);
    let mut first: Option<gtk::ToggleButton> = None;
    let mut slots = Vec::new();
    for edge in Edge::ALL {
        let words = format!("{phone} is {} this screen", edge.words());
        let button = gtk::ToggleButton::builder()
            .icon_name("desklink-device-phone-symbolic")
            .tooltip_text(&words)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["circular", "glide-slot"])
            .active(current.unwrap_or(Edge::Right) == edge)
            .build();
        button.update_property(&[gtk::accessible::Property::Label(&words)]);
        match first.as_ref() {
            Some(group) => button.set_group(Some(group)),
            None => first = Some(button.clone()),
        }
        let (row, column) = match edge {
            Edge::Left => (1, 0),
            Edge::Right => (1, 2),
            Edge::Top => (0, 1),
            Edge::Bottom => (2, 1),
        };
        grid.attach(&button, column, row, 1, 1);
        slots.push((edge, button.clone()));
        let (daemon, id, switch) = (daemon.clone(), device.id.clone(), switch.downgrade());
        button.connect_toggled(move |button| {
            if button.is_active() && switch.upgrade().is_some_and(|switch| switch.is_active()) {
                daemon.send(DaemonCommand::SetGlide(id.clone(), Some(edge)));
            }
        });
    }
    let arrangement = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&grid)
        .visible(current.is_some())
        .build();
    arrangement.update_property(&[gtk::accessible::Property::Label(&format!(
        "Where {phone} sits"
    ))]);
    // Its visibility follows the glide switch, never the search's.
    arrangement.set_widget_name(OWN_VISIBILITY);
    group.add(&arrangement);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    switch.connect_active_notify(move |switch| {
        arrangement.set_visible(switch.is_active());
        if SYNCING.with(|syncing| syncing.get()) {
            return;
        }
        // The slot shown as chosen (right by default).
        let edge = switch.is_active().then(|| {
            slots
                .iter()
                .find(|(_, button)| button.is_active())
                .map_or(Edge::Right, |(edge, _)| *edge)
        });
        daemon.send(DaemonCommand::SetGlide(id.clone(), edge));
    });
    // On here, off on the phone: said, with the ask where the phone takes it.
    if current.is_some() && crate::device_links::glide::phone_allows(&device.id) == Some(false) {
        let row = adw::ActionRow::builder()
            .title(esc(format!("Glide is off on {phone}. Turn it on there.")))
            .subtitle(esc(format!(
                "On {phone}: DeskLink, Settings, Control, “Let {} control this phone”.",
                crate::device_links::glide::desk_name()
            )))
            .build();
        row.add_prefix(&icon_square(
            "desklink-device-phone-symbolic",
            30,
            "row-icon",
        ));
        if crate::device_links::glide::can_ask_phone(&device.incoming_capabilities) {
            let ask = gtk::Button::builder()
                .label("Ask on phone")
                .valign(gtk::Align::Center)
                .build();
            let id = device.id.clone();
            ask.connect_clicked(move |button| {
                crate::device_links::glide::ask_phone(&id);
                button.set_label("Asked");
                button.set_sensitive(false);
            });
            row.add_suffix(&ask);
        }
        group.add(&row);
    }
    if status.asking {
        let row = adw::ActionRow::builder()
            .title("Waiting for your OK on this computer")
            .subtitle("Approve “Capture Input” in the window that just opened.")
            .build();
        row.add_prefix(&icon_square("dialog-information-symbolic", 30, "row-icon"));
        group.add(&row);
    } else if let Some(problem) = status.problem.as_deref() {
        let row = adw::ActionRow::builder()
            .title(if status.can_ask_again {
                "Glide is paused"
            } else {
                "Glide is not working"
            })
            .subtitle(esc(problem))
            .build();
        row.add_prefix(&icon_square("dialog-warning-symbolic", 30, "row-icon"));
        if status.can_ask_again {
            let ask = gtk::Button::builder()
                .label("Turn on again")
                .valign(gtk::Align::Center)
                .build();
            ask.connect_clicked(|_| crate::device_links::glide::ask_again());
            row.add_suffix(&ask);
        }
        group.add(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Searching settings finds a row by its title or its words, hides the
    /// rest and any group left empty, and gives every row back when the
    /// search is cleared; a row hidden on purpose never comes back.
    /// (Needs a display.)
    #[test]
    fn searching_settings_filters_rows_and_keeps_hidden_ones_hidden() {
        crate::test_display::run(|| {
            let row = |title: &str, subtitle: &str| {
                adw::ActionRow::builder()
                    .title(title)
                    .subtitle(subtitle)
                    .build()
            };
            let (wifi, battery, secret) = (
                row("Keep awake", "While a phone is connected"),
                row("Start at Login", "Keeps this computer reachable"),
                row("Hidden row", "Not for this machine"),
            );
            secret.set_visible(false);
            let first = group("Connection");
            first.add(&wifi);
            first.add(&secret);
            let second = group("Appearance");
            second.add(&battery);
            let groups = searchable(&[first.clone(), second.clone()]);

            assert!(filter_groups(&groups, "reachable"));
            assert!(!wifi.is_visible() && battery.is_visible());
            assert!(!first.is_visible(), "a group with nothing to show goes");
            assert!(second.is_visible());
            assert!(filter_groups(&groups, "connection"), "a group's own title");
            assert!(wifi.is_visible() && !secret.is_visible());
            assert!(!filter_groups(&groups, "zzz"), "nothing matches");
            assert!(filter_groups(&groups, ""));
            assert!(wifi.is_visible() && battery.is_visible());
            assert!(!secret.is_visible(), "hidden on purpose stays hidden");
            assert!(first.is_visible() && second.is_visible());
        });
    }

    /// Computer-wide switches on a per-phone page say they are (J18).
    #[test]
    fn computer_wide_rows_say_they_apply_to_every_phone() {
        for text in [
            COMPUTER_WIDE_CLIPBOARD_SUBTITLE,
            COMPUTER_WIDE_MUSIC_SUBTITLE,
        ] {
            assert!(text.contains("every paired phone"), "{text}");
        }
        assert!(COMPUTER_WIDE_CLIPBOARD_TITLE.contains("this computer"));
    }

    /// The groups, in the order the phone's Settings uses too.
    /// Every group title survives being read as markup, and so does a name
    /// with the characters markup uses (a file, a contact, a phone).
    #[test]
    fn titles_and_names_with_markup_characters_read_as_typed() {
        for title in GROUPS
            .iter()
            .copied()
            .chain(["a&b<c>.txt", "Pixel & <9> \"x\"", "R&D's"])
        {
            let escaped = glib::markup_escape_text(title);
            let (_, text, _) = gtk::pango::parse_markup(&escaped, '\0')
                .unwrap_or_else(|error| panic!("{title:?} did not parse: {error}"));
            assert_eq!(text.as_str(), title);
        }
        // Raw, the heading that went missing is not markup at all.
        assert!(gtk::pango::parse_markup("Privacy & security", '\0').is_err());
    }

    /// With a display: a row given `esc`aped text shows it as typed, in the
    /// title, the subtitle and a group's description, with no markup warning.
    /// (Skipped without a display.)
    #[test]
    fn escaped_row_text_reads_as_typed() {
        crate::test_display::run(|| {
            let name = "a&b<c>.txt";
            let row = adw::SwitchRow::builder()
                .title(esc(format!("Show {name}")))
                .subtitle(esc(format!("Tell {name}")))
                .build();
            let label = |text: &str| {
                gtk::pango::parse_markup(text, '\0')
                    .map(|(_, plain, _)| plain.to_string())
                    .unwrap()
            };
            assert_eq!(label(&row.title()), format!("Show {name}"));
            assert_eq!(
                label(&row.subtitle().unwrap_or_default()),
                format!("Tell {name}")
            );
            let group = adw::PreferencesGroup::builder()
                .title(esc("Privacy & security"))
                .description(esc(format!("Answer {name}'s calls")))
                .build();
            assert_eq!(label(&group.title()), "Privacy & security");
        });
    }

    #[test]
    fn settings_has_the_ten_groups_in_the_agreed_order() {
        assert_eq!(
            GROUPS,
            [
                "Connection",
                "Notifications",
                "Clipboard",
                "Calls",
                "Sound",
                "Control",
                "Files",
                "Privacy & security",
                "Appearance",
                "Advanced",
            ]
        );
    }

    /// The rows of each group, in order (nested rows of an expander are not
    /// listed), with markup read.
    fn row_titles(group: &adw::PreferencesGroup) -> Vec<String> {
        fn walk(widget: &gtk::Widget, titles: &mut Vec<String>) {
            if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
                let title = row.title();
                titles.push(
                    gtk::pango::parse_markup(&title, '\0')
                        .map_or_else(|_| title.to_string(), |(_, plain, _)| plain.to_string()),
                );
                return;
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                walk(&next, titles);
                child = next.next_sibling();
            }
        }
        let mut titles = Vec::new();
        walk(group.upcast_ref(), &mut titles);
        titles
    }

    /// The desk's Settings are the reference for the phone's: the titles of
    /// every group, in order, are the table in `docs/protocol/stageD-protocol.md`
    /// ("W7 - settings parity (rows)"). Changing a row here means changing
    /// that table and telling the phone. (Skipped without a display.)
    #[test]
    fn each_settings_group_holds_the_rows_the_contract_lists() {
        crate::test_display::run(|| {
            let info = crate::device_links::device_info::DeviceInfo::local(
                "parity-phone".into(),
                "Pixel 9".into(),
            );
            let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
            let status = crate::device_links::glide::Status {
                availability: crate::device_links::glide::Availability::Unavailable("test"),
                gliding: None,
                problem: None,
                asking: false,
                can_ask_again: false,
                ready: None,
            };
            let (daemon, _commands) = DaemonHandle::detached();
            let groups = groups(&daemon, &AppSettings::default(), &device, &status, false);
            // The table is `testdata/settings_rows.json`, which the contract
            // doc mirrors; the Appearance group has two GNOME-only rows.
            let table: serde_json::Value =
                serde_json::from_str(include_str!("../../testdata/settings_rows.json")).unwrap();
            let table: Vec<(String, Vec<String>)> = table["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|group| {
                    let names = |key: &str| -> Vec<String> {
                        group[key]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|row| row.as_str().unwrap().replace("{phone}", "Pixel 9"))
                            .collect()
                    };
                    let mut rows = names("rows");
                    // Rows the phone has no counterpart for are marked in the
                    // table, so the phone's copy of it can leave them out.
                    for only in names("deskOnlyRows") {
                        assert!(rows.contains(&only), "{only} is not a row");
                    }
                    if crate::shell_dbus::extension_possible() {
                        rows.extend(names("gnomeRows"));
                    }
                    (group["title"].as_str().unwrap().to_string(), rows)
                })
                .collect();
            assert_eq!(groups.len(), table.len());
            for (group, (title, rows)) in groups.iter().zip(&table) {
                let shown = gtk::pango::parse_markup(&group.title(), '\0')
                    .map(|(_, plain, _)| plain.to_string())
                    .unwrap();
                assert_eq!(&shown, title);
                // The relay's read-only status line (compiled in only) is not
                // a setting, so the contract does not list it.
                let mut shown_rows = row_titles(group);
                shown_rows.retain(|row| row != "Away from home");
                assert_eq!(&shown_rows, rows, "rows of {title}");
            }
        });
    }

    #[test]
    fn the_relay_reads_as_its_host() {
        assert_eq!(
            relay_host("wss://relay.desklink.app/v1"),
            "relay.desklink.app"
        );
        assert_eq!(relay_host("wss://10.0.0.2:8443?x=1"), "10.0.0.2:8443");
        assert_eq!(relay_host("relay.example"), "relay.example");
    }

    #[test]
    fn the_glide_row_says_what_to_turn_on_until_the_phone_allows_it() {
        let says = "Turn on “Let Desk control this phone” on the phone first.";
        assert_eq!(glide_explained("Pixel", None, "Desk"), says);
        assert_eq!(glide_explained("Pixel", Some(false), "Desk"), says);
        assert!(glide_explained("Pixel", Some(true), "Desk").starts_with("Move the pointer past"));
    }

    /// Each of the ten groups has one place in the list, in the groups' order.
    #[test]
    fn every_group_has_one_place_in_the_list() {
        let indexes: Vec<usize> = PAGES.iter().map(|page| page.1).collect();
        assert_eq!(indexes, (0..GROUPS.len()).collect::<Vec<_>>());
        assert_eq!(section_label(Section::Computer, "Pixel"), "This computer");
        assert_eq!(section_label(Section::Phone, "Pixel"), "Share with Pixel");
        assert_eq!(
            section_label(Section::Privacy, "Pixel"),
            "Privacy and safety"
        );
    }

    fn view_for_tests() -> (View, DaemonHandle) {
        let info = crate::device_links::device_info::DeviceInfo::local(
            "parity-phone".into(),
            "Pixel 9".into(),
        );
        let device = DeviceView::from_info(&info, "192.168.1.9".into(), true);
        let status = crate::device_links::glide::Status {
            availability: crate::device_links::glide::Availability::Unavailable("test"),
            gliding: None,
            problem: None,
            asking: false,
            can_ask_again: false,
            ready: None,
        };
        let (daemon, _commands) = DaemonHandle::detached();
        let view = build_view(
            None,
            &daemon,
            &AppSettings::default(),
            &device,
            &status,
            false,
        );
        (view, daemon)
    }

    fn all_titles(widget: &gtk::Widget, out: &mut Vec<String>) {
        if let Some(row) = widget.downcast_ref::<adw::PreferencesRow>() {
            let title = row.title();
            out.push(
                gtk::pango::parse_markup(&title, '\0')
                    .map_or_else(|_| title.to_string(), |(_, plain, _)| plain.to_string()),
            );
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            all_titles(&next, out);
            child = next.next_sibling();
        }
    }

    /// In either build configuration no row turns the relay or TURN on or
    /// off: compiled in, one read-only status line; compiled out, nothing.
    /// (Needs a display.)
    #[test]
    fn no_build_has_a_relay_or_turn_toggle() {
        crate::test_display::run(|| {
            assert!(relay_status_row(false).is_none());
            let row = relay_status_row(true).expect("a status line when compiled in");
            assert!(!row.is_activatable(), "it opens nothing");
            fn has_control(widget: &gtk::Widget) -> bool {
                widget.is::<gtk::Switch>()
                    || widget.is::<gtk::CheckButton>()
                    || widget.is::<gtk::Entry>()
                    || widget.is::<gtk::Button>()
                    || {
                        let mut child = widget.first_child();
                        let mut found = false;
                        while let Some(next) = child {
                            found |= has_control(&next);
                            child = next.next_sibling();
                        }
                        found
                    }
            }
            assert!(!has_control(row.upcast_ref()), "no switch, field or button");
        });
    }

    /// The redesign moves rows into subpages and removes none: every row title
    /// of the desk-ui1 page (`testdata/settings_row_titles_desk_ui1.json`,
    /// written from that page, the Away from home dialog included) is in a
    /// subpage now, and each subpage is linked from the list. (Needs a display.)
    #[test]
    fn every_original_settings_row_is_still_reachable() {
        crate::test_display::run(|| {
            // The rows of the old page. Some depend on the machine: the two
            // GNOME rows (an extension is possible) and the relay address
            // row's title (a built-in relay server).
            let table: serde_json::Value = serde_json::from_str(include_str!(
                "../../testdata/settings_row_titles_desk_ui1.json"
            ))
            .unwrap();
            let names = |key: &str| -> Vec<String> {
                table[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|title| title.as_str().unwrap().to_string())
                    .collect()
            };
            let mut before = names("always");
            if crate::shell_dbus::extension_possible() {
                before.extend(names("gnome"));
            }
            // The relay's switch, address, TURN choice and status dialog are
            // gone for good: RELAY_ENABLED is the only control. ("Status" is
            // too plain a title to look for.)
            let mut relay = names("relay");
            relay.extend(names("relayCustom"));
            relay.extend(names("relayBuiltIn"));
            let (view, _daemon) = view_for_tests();
            let mut after = Vec::new();
            for page in &view.pages {
                all_titles(page.child().unwrap().upcast_ref(), &mut after);
            }
            let shown: Vec<_> = relay
                .iter()
                .filter(|title| *title != "Status" && after.contains(title))
                .collect();
            assert!(shown.is_empty(), "relay toggles shown: {shown:?}");
            if !RELAY_ENABLED {
                assert!(!after.iter().any(|title| title == "Away from home"));
                let page_lines = PAGES.map(|(_, _, _, summary)| summary).join(" ");
                assert!(!page_lines.to_lowercase().contains("away from home"));
            }
            let missing: Vec<_> = before
                .iter()
                .filter(|title| !after.contains(title))
                .collect();
            assert!(missing.is_empty(), "rows lost: {missing:?}");
            // And the list links to every subpage.
            let mut listed = Vec::new();
            let root = view.root.find_page("list").unwrap();
            all_titles(root.child().unwrap().upcast_ref(), &mut listed);
            for title in GROUPS {
                assert!(
                    listed.iter().any(|row| row == title),
                    "{title} is not in the list"
                );
            }
        });
    }

    /// Searching shows the matching rows under their group's name, from every
    /// page, and clearing the search puts each group back on its page.
    #[test]
    fn searching_settings_reaches_every_page_and_clearing_restores_them() {
        crate::test_display::run(|| {
            fn find(widget: &gtk::Widget) -> Option<adw::PreferencesGroup> {
                if let Some(group) = widget.downcast_ref::<adw::PreferencesGroup>() {
                    return Some(group.clone());
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    if let Some(found) = find(&next) {
                        return Some(found);
                    }
                    child = next.next_sibling();
                }
                None
            }
            let (view, _daemon) = view_for_tests();
            let groups: Vec<adw::PreferencesGroup> = view
                .pages
                .iter()
                .map(|page| find(page.child().unwrap().upcast_ref()).unwrap())
                .collect();
            let homes: Vec<_> = groups.iter().map(|group| group.parent()).collect();
            let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
            // `search-changed` is sent after a pause; without a main loop, send it.
            search.set_text("glide");
            search.emit_by_name::<()>("search-changed", &[]);
            assert_ne!(groups[5].parent(), homes[5], "Control moved to the results");
            assert!(groups[5].is_visible(), "and shows its matching rows");
            assert!(!groups[7].is_visible(), "Privacy has no glide row");
            search.set_text("");
            search.emit_by_name::<()>("search-changed", &[]);
            for (group, home) in groups.iter().zip(&homes) {
                assert_eq!(&group.parent(), home);
                assert!(group.is_visible());
            }
        });
    }

    /// The search reads what the rows say, not their markup: an apostrophe or
    /// an ampersand typed is found, and "amp" is not in "Privacy & security".
    /// (Needs a display.)
    #[test]
    fn search_matches_the_plain_words_of_the_rows() {
        crate::test_display::run(|| {
            let privacy = group("Privacy & security");
            let send = adw::SwitchRow::builder()
                .title(esc("Send this computer's notifications"))
                .subtitle(esc("Only from the apps you choose & never back"))
                .build();
            privacy.add(&send);
            let other = group("Sound");
            let mic = adw::SwitchRow::builder().title("Microphone").build();
            other.add(&mic);
            let groups = searchable(&[privacy.clone(), other.clone()]);
            assert!(filter_groups(&groups, "computer's"));
            assert!(send.is_visible() && !mic.is_visible());
            assert!(filter_groups(&groups, "choose & never"));
            assert!(send.is_visible());
            assert!(
                filter_groups(&groups, "privacy & security"),
                "the group's title"
            );
            assert!(!filter_groups(&groups, "amp"), "markup is not text");
            assert!(!filter_groups(&groups, "&amp;"));
            assert!(filter_groups(&groups, ""));
        });
    }

    /// A row whose visibility is its own (the glide arrangement, shown while
    /// glide is on) is not touched by a search and its clearing.
    #[test]
    fn the_search_leaves_a_rows_own_visibility_alone() {
        crate::test_display::run(|| {
            let controls = group("Control");
            let glide = adw::SwitchRow::builder().title("Glide onto Pixel").build();
            let arrangement = adw::PreferencesRow::builder().visible(false).build();
            arrangement.set_widget_name(OWN_VISIBILITY);
            controls.add(&glide);
            controls.add(&arrangement);
            let groups = searchable(std::slice::from_ref(&controls));
            // Glide is turned on after the page was built; then a search and a clear.
            arrangement.set_visible(true);
            assert!(filter_groups(&groups, "glide"));
            assert!(filter_groups(&groups, ""));
            assert!(arrangement.is_visible());
            arrangement.set_visible(false);
            assert!(filter_groups(&groups, "glide"));
            assert!(filter_groups(&groups, ""));
            assert!(!arrangement.is_visible());
        });
    }

    /// A double click on a list row pushes its subpage once: a second push
    /// of the same tag is an Adwaita-CRITICAL (seen on the real desk).
    /// (Needs a display.)
    #[test]
    fn a_second_activation_does_not_push_the_subpage_again() {
        crate::test_display::run(|| {
            let (view, _daemon) = view_for_tests();
            let list = view.root.find_page("list").unwrap();
            let mut rows = Vec::new();
            rows_in(list.upcast_ref(), &mut rows);
            let row = rows
                .iter()
                .find(|row| row.title() == GROUPS[0])
                .expect("a list row for the first group");
            let criticals = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = criticals.clone();
            let handler = glib::log_set_handler(
                Some("Adwaita"),
                glib::LogLevels::LEVEL_CRITICAL,
                false,
                false,
                move |_, _, _| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                },
            );
            row.activate();
            row.activate();
            glib::log_remove_handler(Some("Adwaita"), handler);
            assert_eq!(criticals.load(std::sync::atomic::Ordering::Relaxed), 0);
            assert_eq!(view.root.navigation_stack().n_items(), 2);
            assert!(shows(&view.root, 0));
            OPEN.with(|slot| slot.set(None));
        });
    }

    /// A rebuild keeps what was typed and the subpage that was open, and a
    /// phone's change moved onto a switch is not sent back as a click.
    /// (Needs a display.)
    #[test]
    fn a_rebuild_keeps_the_query_and_the_open_page_and_a_sync_sends_nothing() {
        crate::test_display::run(|| {
            let (first, _daemon) = view_for_tests();
            let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
            search.set_text("clipboard");
            search.emit_by_name::<()>("search-changed", &[]);
            OPEN.with(|slot| slot.set(Some(7)));
            drop(first);
            let (second, _daemon) = view_for_tests();
            let search = SEARCH.with(|slot| slot.borrow().upgrade()).unwrap();
            assert_eq!(search.text(), "clipboard");
            assert_eq!(
                second
                    .root
                    .visible_page()
                    .and_then(|page| page.tag())
                    .as_deref(),
                Some("page-7")
            );
            search.set_text("");
            search.emit_by_name::<()>("search-changed", &[]);
            OPEN.with(|slot| slot.set(None));

            // A switch moved by `sync_live` does not run a click's handler.
            let (daemon, commands) = DaemonHandle::detached();
            let row = adw::SwitchRow::builder().build();
            let sent = std::rc::Rc::new(std::cell::Cell::new(0));
            let counter = sent.clone();
            toggled(&row, move |_| counter.set(counter.get() + 1));
            SYNCING.with(|syncing| syncing.set(true));
            row.set_active(true);
            SYNCING.with(|syncing| syncing.set(false));
            assert_eq!(sent.get(), 0);
            row.set_active(false);
            assert_eq!(sent.get(), 1);
            drop((daemon, commands));
        });
    }
}
