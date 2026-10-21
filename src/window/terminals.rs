//! The Terminals page: every shared terminal at this computer as a dark
//! live tile, searchable, and one terminal's detail — a large live
//! preview, who is attached, and ending it.
//!
//! Output is refreshed in place each second; the grid and the detail are
//! rebuilt only when sessions or attachments change, and the search entry
//! lives outside both, so typing in it is never interrupted.

use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::{confirm, ellipsized, icon_button, named_widgets, text, toast_from, DesklinkWindow};
use crate::device_links::device::DeviceView;
use crate::device_links::pty::registry::SessionInfo;
use crate::device_links::webrtc::terminal_bridge::{attached_devices, end_device_sessions};
use crate::link_glyph::presence_dot;
use crate::terminal_previews::{display_title, preview, screen_lines, sessions, started};

/// How many lines the detail's preview shows.
const DETAIL_LINES: usize = 60;

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    count: gtk::Label,
    detach: gtk::Button,
    grid: gtk::Box,
    note: gtk::Label,
    detail: gtk::Box,
    /// The terminal the detail shows.
    detail_id: RefCell<Option<String>>,
    /// The selected device: id and name.
    device: RefCell<(String, String)>,
    key: RefCell<String>,
    detail_key: RefCell<String>,
    /// The New Terminal menu's "Start in" folders, and what they were.
    folders: gtk::gio::Menu,
    shown_folders: RefCell<Vec<String>>,
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search terminals")
            .width_request(240)
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Search terminals")]);
        let count = text("", &["text-dim"]);
        let detach = super::soft_button("Detach");
        detach.set_css_classes(&["coral-outline"]);
        detach.set_visible(false);
        let header = gtk::Box::builder().spacing(12).build();
        header.set_widget_name("responsive");
        header.append(&search);
        header.append(&count);
        header.append(&gtk::Box::builder().hexpand(true).build());
        header.append(&detach);
        let folders = gtk::gio::Menu::new();
        header.append(&new_terminal_button(&folders));

        let grid = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let note = text("", &["text-faint", "body-small"]);
        note.set_wrap(true);
        let info = gtk::Box::builder().spacing(8).build();
        info.append(
            &gtk::Image::builder()
                .icon_name("dialog-information-symbolic")
                .valign(gtk::Align::Start)
                .css_classes(["text-faint"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        info.append(&note);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(8)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .build();
        list.append(&header);
        list.append(&grid);
        list.append(&info);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let detail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .build();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&scroll, Some("grid"));
        stack.add_named(&detail, Some("detail"));

        let window = win.downgrade();
        search.connect_search_changed(move |_| {
            if let Some(window) = window.upgrade() {
                window.refresh_soon();
            }
        });
        let window = win.downgrade();
        detach.connect_clicked(move |button| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let Some(page) = window
                .imp()
                .terminals
                .borrow()
                .as_ref()
                .map(|page| page.device.borrow().clone())
            else {
                return;
            };
            detach_device(button, &page.0, &page.1);
        });

        Self {
            root: stack.clone().upcast(),
            stack,
            search,
            count,
            detach,
            grid,
            note,
            detail,
            detail_id: RefCell::default(),
            device: RefCell::default(),
            key: RefCell::default(),
            detail_key: RefCell::default(),
            folders,
            shown_folders: RefCell::default(),
        }
    }

    /// The folders New Terminal offers: home, the open sessions' folders,
    /// then the recent ones (the same list the phone gets).
    fn update_folders(&self, all: &[SessionInfo]) {
        let folders = crate::device_links::pty::registry::folders(
            all,
            &crate::device_links::config::app_settings().recent_folders,
        );
        if *self.shown_folders.borrow() == folders {
            return;
        }
        self.folders.remove_all();
        for folder in &folders {
            let label = if folder == "~" { "Home folder" } else { folder };
            let item = gtk::gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(
                Some("win.new-terminal-in"),
                Some(&folder.to_variant()),
            );
            self.folders.append_item(&item);
        }
        self.shown_folders.replace(folders);
    }

    pub(super) fn focus_search(&self) {
        self.close_detail();
        self.search.grab_focus();
    }

    pub(super) fn close_detail(&self) {
        self.detail_id.replace(None);
        self.detail_key.replace(String::new());
        self.stack.set_visible_child_name("grid");
    }

    /// Shows terminal `id`'s detail.
    pub(super) fn open_detail(&self, win: &DesklinkWindow, id: &str) {
        self.detail_id.replace(Some(id.to_string()));
        self.detail_key.replace(String::new());
        self.stack.set_visible_child_name("detail");
        let device = win
            .daemon()
            .map(|daemon| win.known_devices(&daemon))
            .and_then(|devices| {
                let selected = win.imp().selected_device_id.borrow().clone();
                devices
                    .into_iter()
                    .find(|device| Some(&device.id) == selected.as_ref())
            });
        if let Some(device) = device {
            self.update(win, &device);
        }
        win.update_title();
    }

    /// The header's title while the detail is open.
    pub(super) fn detail_title(&self) -> Option<String> {
        let id = self.detail_id.borrow().clone()?;
        sessions()
            .into_iter()
            .find(|info| info.id == id)
            .map(|info| display_title(&info.title))
    }

    pub(super) fn update(&self, win: &DesklinkWindow, device: &DeviceView) {
        self.device
            .replace((device.id.clone(), device.name.clone()));
        let all = sessions();
        self.update_folders(&all);
        let attached: Vec<Vec<String>> =
            all.iter().map(|info| attached_devices(&info.id)).collect();
        let phone_attached = attached.iter().any(|devices| devices.contains(&device.id));
        self.detach.set_label(&format!("Detach {}", device.name));
        self.detach.set_visible(phone_attached);
        self.count.set_label(&format!("{} open", all.len()));
        self.note.set_label(&format!(
            "Terminals keep running after you close this window. {} can open any of them.",
            device.name
        ));

        let open = self.detail_id.borrow().clone();
        if let Some(id) = open {
            match all.iter().position(|info| info.id == id) {
                Some(index) => self.update_detail(win, device, &all[index], &attached[index]),
                None => {
                    // Ended from the phone, or it exited and was reaped.
                    self.close_detail();
                    win.update_title();
                }
            }
        }

        let query = self.search.text().trim().to_lowercase();
        let shown: Vec<(&SessionInfo, &Vec<String>)> = all
            .iter()
            .zip(&attached)
            .filter(|(info, _)| {
                query.is_empty()
                    || display_title(&info.title).to_lowercase().contains(&query)
                    || info.title.to_lowercase().contains(&query)
            })
            .collect();
        let key = serde_json::to_string(&(
            shown
                .iter()
                .map(|(info, attached)| {
                    (&info.id, &info.title, info.exit_status, attached, &info.cwd)
                })
                .collect::<Vec<_>>(),
            &device.id,
            &device.name,
            all.is_empty(),
        ))
        .unwrap_or_default();
        if *self.key.borrow() != key {
            self.key.replace(key);
            while let Some(child) = self.grid.first_child() {
                self.grid.remove(&child);
            }
            if all.is_empty() {
                self.grid.append(&empty_tile());
            } else if shown.is_empty() {
                self.grid
                    .append(&text("No terminal matches your search.", &["text-dim"]));
            } else {
                let flow = gtk::FlowBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .homogeneous(true)
                    .min_children_per_line(1)
                    .max_children_per_line(if win.imp().narrow.get() { 1 } else { 3 })
                    .column_spacing(14)
                    .row_spacing(14)
                    .build();
                flow.set_widget_name("terminal-grid");
                for (info, attached) in &shown {
                    let tile = tile(win, info, attached, device);
                    flow.append(&tile);
                    if let Some(child) = tile.parent() {
                        child.set_focusable(false);
                    }
                }
                self.grid.append(&flow);
            }
        }
        self.refresh_lines();
    }

    /// Each tile's and the detail's latest output, in place.
    fn refresh_lines(&self) {
        let all = sessions();
        for widget in named_widgets(self.root.upcast_ref(), "terminal-lines:") {
            let id = widget
                .widget_name()
                .trim_start_matches("terminal-lines:")
                .to_string();
            let Some(info) = all.iter().find(|info| info.id == id) else {
                continue;
            };
            let (lines, _) = preview(info);
            set_text(&widget, &tile_text(&lines, info.exit_status));
        }
        for widget in named_widgets(self.root.upcast_ref(), "terminal-detail:") {
            let id = widget
                .widget_name()
                .trim_start_matches("terminal-detail:")
                .to_string();
            let Some(info) = all.iter().find(|info| info.id == id) else {
                continue;
            };
            let (lines, _) = screen_lines(info, DETAIL_LINES);
            let mut shown = lines.join("\n");
            if let Some(status) = info.exit_status {
                shown.push_str(&format!("\n\n[Exited ({status})]"));
            }
            set_text(&widget, if shown.is_empty() { " " } else { &shown });
        }
    }

    fn update_detail(
        &self,
        win: &DesklinkWindow,
        device: &DeviceView,
        info: &SessionInfo,
        attached: &[String],
    ) {
        let key = serde_json::to_string(&(
            &info.id,
            &info.title,
            info.exit_status,
            info.desk_window,
            attached,
            &info.cwd,
            &device.id,
            &device.name,
        ))
        .unwrap_or_default();
        if *self.detail_key.borrow() == key {
            return;
        }
        self.detail_key.replace(key);
        while let Some(child) = self.detail.first_child() {
            self.detail.remove(&child);
        }
        self.detail
            .append(&detail_view(win, device, info, attached));
        win.apply_layout();
    }
}

fn set_text(widget: &gtk::Widget, value: &str) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        if label.label() != value {
            label.set_label(value);
        }
    }
}

/// What a tile shows: its last three lines, or why the session ended.
fn tile_text(lines: &[String], exit_status: Option<u32>) -> String {
    match exit_status {
        Some(status) => format!("Exited ({status})"),
        None if lines.is_empty() => "❯".to_string(),
        None => lines[lines.len().saturating_sub(3)..].join("\n"),
    }
}

/// "New Terminal" (a shell); its menu also offers the installed agents.
fn new_terminal_button(folders: &gtk::gio::Menu) -> adw::SplitButton {
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Shell"), Some("win.new-terminal::shell"));
    let agents = crate::terminal_previews::installed_agents();
    if agents.iter().any(|agent| agent == "claude") {
        menu.append(Some("Claude Code"), Some("win.new-terminal::claude"));
    }
    if agents.iter().any(|agent| agent == "codex") {
        menu.append(Some("Codex"), Some("win.new-terminal::codex"));
    }
    let menu = {
        let sections = gtk::gio::Menu::new();
        sections.append_section(None, &menu);
        sections.append_section(Some("Start in"), folders);
        sections
    };
    let content = adw::ButtonContent::builder()
        .icon_name("list-add-symbolic")
        .label("New Terminal")
        .build();
    let button = adw::SplitButton::builder()
        .child(&content)
        .menu_model(&menu)
        .action_name("win.new-terminal")
        .action_target(&"shell".to_variant())
        .tooltip_text("Start a shared terminal here")
        .dropdown_tooltip("More kinds of terminal, and where to start")
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label("New Terminal")]);
    button.set_widget_name("new-terminal");
    button
}

/// Detaches `device` from every terminal, after asking.
fn detach_device(parent: &impl IsA<gtk::Widget>, device_id: &str, name: &str) {
    let (id, target) = (device_id.to_string(), parent.as_ref().clone());
    let name_owned = name.to_string();
    confirm(
        parent,
        &format!("Detach {name}?"),
        "The terminals keep running here, and the phone can open them again.",
        "_Detach",
        move || {
            end_device_sessions(&id);
            toast_from(&target, &format!("Detached {name_owned}"));
            if let Some(window) = target.root().and_downcast::<DesklinkWindow>() {
                window.refresh_soon();
            }
        },
    );
}

/// One session's dark tile: title, started time, its last lines, and who
/// has it open. A phone attached gives it a coral edge.
fn tile(
    win: &DesklinkWindow,
    info: &SessionInfo,
    attached: &[String],
    device: &DeviceView,
) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    let head = gtk::Box::builder().spacing(8).build();
    let title = ellipsized(&display_title(&info.title), &["mono", "tile-title"]);
    title.set_max_width_chars(14);
    title.set_hexpand(true);
    head.append(&title);
    head.append(&text(
        &started(&info.id).unwrap_or_default(),
        &["mono", "tile-meta"],
    ));
    content.append(&head);
    if let Some(folder) = &info.cwd {
        let place = ellipsized(
            &match &info.shell {
                Some(shell) => format!("{folder} · {shell}"),
                None => folder.clone(),
            },
            &["mono", "tile-meta"],
        );
        place.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        content.append(&place);
    }

    let (lines, _) = preview(info);
    let body = gtk::Label::builder()
        .label(tile_text(&lines, info.exit_status))
        .xalign(0.0)
        .yalign(0.0)
        .max_width_chars(24)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .vexpand(true)
        .css_classes(["mono", "tile-lines"])
        .build();
    body.set_widget_name(&format!("terminal-lines:{}", info.id));
    content.append(&body);

    let foot = gtk::Box::builder()
        .spacing(8)
        .css_classes(["tile-foot"])
        .build();
    let here = attached.contains(&device.id);
    let others = attached.iter().filter(|id| **id != device.id).count();
    if here {
        foot.append(&presence_dot(&format!("{} is attached", device.name)));
        foot.append(&ellipsized(
            &format!("{} attached", device.name),
            &["tile-phone"],
        ));
    } else if others > 0 {
        foot.append(&presence_dot("Another device is attached"));
        foot.append(&ellipsized("Another device attached", &["tile-phone"]));
    } else {
        foot.append(&ellipsized("Only on this computer", &["tile-meta"]));
    }
    foot.append(&gtk::Box::builder().hexpand(true).build());
    foot.append(
        &gtk::Image::builder()
            .icon_name("go-next-symbolic")
            .css_classes(["tile-meta"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    content.append(&foot);

    let mut classes = vec!["terminal-tile"];
    if !attached.is_empty() {
        classes.push("attached");
    }
    let button = gtk::Button::builder()
        .child(&content)
        .height_request(176)
        .vexpand(false)
        .valign(gtk::Align::Start)
        .css_classes(classes)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{} terminal{}",
        display_title(&info.title),
        if here {
            format!(", {} attached", device.name)
        } else {
            String::new()
        }
    ))]);
    let (window, id) = (win.downgrade(), info.id.clone());
    button.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            if let Some(page) = window.imp().terminals.borrow().as_ref() {
                page.open_detail(&window, &id);
            }
        }
    });
    button
}

/// No sessions yet: a dark tile with a blinking cursor and the ways to
/// start one.
fn empty_tile() -> gtk::Box {
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .css_classes(["terminal-card"])
        .build();
    let prompt = gtk::Box::builder().spacing(8).build();
    prompt.append(&text("❯", &["mono", "terminal-prompt"]));
    prompt.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["terminal-cursor"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    tile.append(&prompt);
    let words = text(
        "Start Claude Code, Codex or a shell. The phone sees it too.",
        &["terminal-title"],
    );
    words.set_wrap(true);
    tile.append(&words);
    let buttons = gtk::Box::builder().spacing(8).build();
    let agents = crate::terminal_previews::installed_agents();
    for (label, program) in [
        ("Shell", "shell"),
        ("Claude Code", "claude"),
        ("Codex", "codex"),
    ] {
        if program != "shell" && !agents.iter().any(|agent| agent == program) {
            continue;
        }
        buttons.append(
            &gtk::Button::builder()
                .label(label)
                .action_name("win.new-terminal")
                .action_target(&program.to_variant())
                .css_classes(["dark-chip"])
                .build(),
        );
    }
    tile.append(&buttons);
    tile
}

/// One terminal: its live screen on the left, who is attached and what can
/// be done on the right (below, when narrow).
fn detail_view(
    win: &DesklinkWindow,
    device: &DeviceView,
    info: &SessionInfo,
    attached: &[String],
) -> gtk::Box {
    let split = gtk::Box::builder().vexpand(true).build();
    split.set_widget_name("responsive");

    let screen = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["term-detail"])
        .build();
    let bar = gtk::Box::builder()
        .spacing(10)
        .css_classes(["term-detail-bar"])
        .build();
    let back = icon_button("go-previous-symbolic", "Terminals", &["term-back"]);
    let window = win.downgrade();
    back.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            if let Some(page) = window.imp().terminals.borrow().as_ref() {
                page.close_detail();
            }
            window.update_title();
        }
    });
    bar.append(&back);
    let title = ellipsized(&display_title(&info.title), &["mono", "term-detail-title"]);
    title.set_hexpand(true);
    bar.append(&title);
    if attached.contains(&device.id) {
        let pill = gtk::Box::builder()
            .spacing(8)
            .css_classes(["coral-pill"])
            .build();
        pill.append(&presence_dot(&format!("{} is here", device.name)));
        pill.append(&gtk::Label::new(Some(&format!("{} is here", device.name))));
        bar.append(&pill);
    }
    screen.append(&bar);
    let lines = gtk::Label::builder()
        .label(" ")
        .xalign(0.0)
        .yalign(1.0)
        .valign(gtk::Align::Fill)
        .vexpand(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .selectable(true)
        .css_classes(["mono", "term-detail-lines"])
        .build();
    lines.set_widget_name(&format!("terminal-detail:{}", info.id));
    lines.update_property(&[gtk::accessible::Property::Label("Terminal output")]);
    screen.append(&lines);
    split.append(&screen);

    let side = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .width_request(250)
        .css_classes(["detail-side"])
        .build();
    let who = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    who.append(&text("Attached", &["side-heading"]));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list", "compact-list"])
        .build();
    list.append(&attached_row(
        "computer-symbolic",
        "This computer",
        if info.desk_window {
            "In a terminal window"
        } else {
            "No window open"
        },
        None,
        None,
    ));
    let names: std::collections::HashMap<String, String> = win
        .daemon()
        .map(|daemon| {
            daemon
                .devices()
                .into_iter()
                .map(|known| (known.id, known.name))
                .collect()
        })
        .unwrap_or_default();
    for id in attached {
        let name = names
            .get(id)
            .cloned()
            .unwrap_or_else(|| "A phone".to_string());
        let detach = gtk::Button::builder()
            .label("Detach")
            .valign(gtk::Align::Center)
            .tooltip_text(format!("Detach {name} from its terminals"))
            .css_classes(["flat", "coral-link"])
            .build();
        let (device_id, device_name) = (id.clone(), name.clone());
        detach.connect_clicked(move |button| detach_device(button, &device_id, &device_name));
        list.append(&attached_row(
            "phone-symbolic",
            &name,
            "Viewing",
            Some(&detach),
            Some(&presence_dot(&format!("{name} is attached"))),
        ));
    }
    who.append(&list);
    if !attached.contains(&device.id) {
        let note = text(
            &format!(
                "Not open on {}. It can join from its Terminals tab.",
                device.name
            ),
            &["text-faint", "body-small"],
        );
        note.set_wrap(true);
        who.append(&note);
    }
    side.append(&who);

    let facts = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list", "compact-list"])
        .build();
    for (label, value) in [
        (
            "Folder",
            info.cwd.clone().unwrap_or_else(|| "—".to_string()),
        ),
        (
            "Shell",
            info.shell
                .clone()
                .unwrap_or_else(|| info.title.trim_start_matches('-').to_string()),
        ),
        ("Started", started(&info.id).unwrap_or_default()),
        ("Size", format!("{}×{}", info.cols, info.rows)),
    ] {
        let row = gtk::Box::builder()
            .spacing(12)
            .css_classes(["fact-row"])
            .build();
        let name = text(label, &["text-dim", "body-small"]);
        name.set_hexpand(true);
        row.append(&name);
        row.append(&ellipsized(&value, &["mono", "body-small"]));
        facts.append(
            &gtk::ListBoxRow::builder()
                .activatable(false)
                .child(&row)
                .build(),
        );
    }
    side.append(&facts);
    side.append(&gtk::Box::builder().vexpand(true).build());

    let open = icon_button(
        "window-new-symbolic",
        "Open in Terminal Window",
        &["soft-button"],
    );
    let id = info.id.clone();
    open.connect_clicked(move |_| crate::device_links::pty::desk::open_on_desktop(&id));
    side.append(&open);
    let end = gtk::Button::builder()
        .label("End Session")
        .css_classes(["danger-soft"])
        .build();
    let (id, name) = (info.id.clone(), display_title(&info.title));
    end.connect_clicked(move |button| {
        let (id, target) = (id.clone(), button.clone());
        confirm(
            button,
            &format!("End {name}?"),
            "Whatever runs in it stops, here and on every phone.",
            "_End Session",
            move || {
                crate::terminal_previews::close(&id);
                toast_from(&target, "Session ended");
                if let Some(window) = target.root().and_downcast::<DesklinkWindow>() {
                    if let Some(page) = window.imp().terminals.borrow().as_ref() {
                        page.close_detail();
                    }
                    window.update_title();
                    window.refresh_soon();
                }
            },
        );
    });
    side.append(&end);
    split.append(&side);
    split
}

fn attached_row(
    icon: &str,
    title: &str,
    subtitle: &str,
    end: Option<&gtk::Button>,
    dot: Option<&gtk::Box>,
) -> gtk::ListBoxRow {
    let row = gtk::Box::builder()
        .spacing(10)
        .css_classes(["attached-row"])
        .build();
    row.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .css_classes(["text-dim"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(1)
        .hexpand(true)
        .build();
    let name = gtk::Box::builder().spacing(6).build();
    name.append(&ellipsized(title, &["heading", "body-small"]));
    if let Some(dot) = dot {
        name.append(dot);
    }
    words.append(&name);
    words.append(&ellipsized(subtitle, &["text-dim", "caption"]));
    row.append(&words);
    if let Some(end) = end {
        row.append(end);
    }
    gtk::ListBoxRow::builder()
        .activatable(false)
        .child(&row)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tile_shows_three_lines_or_why_it_ended() {
        let lines: Vec<String> = ["a", "b", "c", "d"].iter().map(|l| l.to_string()).collect();
        assert_eq!(tile_text(&lines, None), "b\nc\nd");
        assert_eq!(tile_text(&[], None), "❯");
        assert_eq!(tile_text(&lines, Some(1)), "Exited (1)");
    }
}
