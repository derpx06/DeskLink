//! The Automations page (F7): rules that run by themselves, "When … If …
//! Do …", with templates, a test run and each rule's last runs; and the
//! editor dialog.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use serde_json::{Map, Value};

use super::{icon_button, text, toast_from, DesklinkWindow};
use crate::device_links::automations::{
    self, Kind, Param, Rule, Run, Side, Step, ACTIONS, CONDITIONS, TRIGGERS,
};
use crate::device_links::device::DeviceView;

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    let key = format!(
        "{}\n{}\n{}",
        automations::generation(),
        device.name,
        super::clock_time(glib::DateTime::now_local().ok())
    );
    if !win.page_changed("automations", key) {
        return;
    }
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(8)
        .margin_bottom(28)
        .margin_start(16)
        .margin_end(16)
        .build();

    let head = gtk::Box::builder().spacing(12).build();
    head.set_widget_name("responsive");
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    words.append(&text("Rules that run by themselves", &["heading"]));
    let explained = text(
        &format!(
            "When something happens here or on {}, if what you choose holds, do the steps \
             you list. Each rule runs on the computer or on the phone, as you choose.",
            device.name
        ),
        &["text-dim", "body-small"],
    );
    explained.set_wrap(true);
    words.append(&explained);
    head.append(&words);
    head.append(&new_rule_button(win, device));
    page.append(&head);

    let rules = automations::rules();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for rule in &rules {
        list.append(&rule_row(win, device, rule));
    }
    page.append(&list);
    if rules.iter().all(Rule::is_builtin) {
        let empty = text(
            "No rules of your own yet. Start from a template with New Rule.",
            &["text-faint", "body-small"],
        );
        empty.set_wrap(true);
        page.append(&empty);
    }
    win.replace_body(
        "automations",
        &adw::Clamp::builder().maximum_size(820).child(&page).build(),
    );
}

/// "New Rule ▾": from scratch, or from a template.
fn new_rule_button(win: &DesklinkWindow, device: &DeviceView) -> gtk::MenuButton {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    for (title, rule) in templates(&device.name) {
        let button = gtk::Button::builder()
            .label(title.as_str())
            .css_classes(["flat"])
            .build();
        if let Some(label) = button.child().and_downcast::<gtk::Label>() {
            label.set_xalign(0.0);
        }
        let (win, popover) = (win.downgrade(), popover.clone());
        button.connect_clicked(move |_| {
            popover.popdown();
            if let Some(win) = win.upgrade() {
                edit(&win, rule.clone());
            }
        });
        menu.append(&button);
    }
    let content = gtk::Box::builder().spacing(7).build();
    content.append(&gtk::Image::from_icon_name("list-add-symbolic"));
    content.append(&gtk::Label::new(Some("New Rule")));
    let button = gtk::MenuButton::builder()
        .child(&content)
        .popover(&popover)
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label("New Rule")]);
    button
}

fn blank(trigger: Step, actions: Vec<Step>) -> Rule {
    Rule {
        id: String::new(),
        name: String::new(),
        enabled: true,
        trigger,
        conditions: Vec::new(),
        actions,
        run_on: Side::Desk,
        modified: 0,
        deleted: false,
    }
}

/// The editor's starting points (A7's built-in is listed as a rule).
pub(super) fn templates(phone: &str) -> Vec<(String, Rule)> {
    let first_command = crate::device_links::config::app_settings()
        .run_commands
        .first()
        .map(|command| command.id.clone())
        .unwrap_or_default();
    let mut home = blank(
        Step::new("phone-connected"),
        vec![Step::new("keep-awake").with("on", true)],
    );
    home.conditions = vec![Step::new("wifi").with("ssid", "")];
    home.name = format!("When {phone} connects at home, keep awake on");
    let mut leave = blank(Step::new("phone-left"), vec![Step::new("lock-desk")]);
    leave.name = "When I leave, lock this computer".into();
    let mut nfc = blank(
        Step::new("nfc").with("tag", "Desk"),
        vec![Step::new("run-command").with("commandId", first_command)],
    );
    nfc.name = "Tap the NFC tag to start my command".into();
    vec![
        (
            "Start from scratch".into(),
            blank(Step::new("phone-connected"), Vec::new()),
        ),
        (leave.name.clone(), leave),
        (home.name.clone(), home),
        (nfc.name.clone(), nfc),
    ]
}

/// One rule: its switch, and when expanded its steps, Test Run, Edit,
/// Delete and last runs.
fn rule_row(win: &DesklinkWindow, device: &DeviceView, rule: &Rule) -> adw::ExpanderRow {
    let supported = rule.supported();
    let mut subtitle = if rule.name.trim().is_empty() {
        String::new()
    } else {
        rule.summary()
    };
    let place = match rule.run_on {
        Side::Desk => "Runs on this computer".to_string(),
        Side::Phone => format!("Runs on {}", device.name),
    };
    let note = if rule.is_builtin() {
        "Built in".to_string()
    } else if automations::needs_approval(rule) {
        format!(
            "Made on {} · approve it to let it act on this computer",
            device.name
        )
    } else if !supported {
        "Needs a newer DeskLink".to_string()
    } else {
        place
    };
    subtitle = if subtitle.is_empty() {
        note
    } else {
        format!("{subtitle}\n{note}")
    };
    let row = adw::ExpanderRow::builder()
        .title(rule.label())
        .subtitle(&subtitle)
        .use_markup(false)
        .subtitle_lines(3)
        .build();
    row.set_widget_name("rule-row");
    row.add_prefix(&super::icon_square(
        if rule.is_builtin() {
            "call-start-symbolic"
        } else {
            "system-run-symbolic"
        },
        30,
        "row-icon",
    ));
    let switch = gtk::Switch::builder()
        .active(rule.enabled)
        .valign(gtk::Align::Center)
        .sensitive(supported)
        .build();
    switch.update_property(&[gtk::accessible::Property::Label(&format!(
        "{} on",
        rule.label()
    ))]);
    let id = rule.id.clone();
    switch.connect_state_set(move |_, on| {
        automations::set_enabled(&id, on);
        glib::Propagation::Proceed
    });
    row.add_suffix(&switch);

    let actions = gtk::Box::builder()
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    let test = icon_button(
        "media-playback-start-symbolic",
        "Test Run",
        &["soft-button"],
    );
    test.set_sensitive(supported);
    let id = rule.id.clone();
    test.connect_clicked(move |button| {
        automations::test_run(&id);
        toast_from(button, "Running the rule’s steps now");
    });
    actions.append(&test);
    if automations::needs_approval(rule) {
        let approve = icon_button("object-select-symbolic", "Approve", &["suggested-action"]);
        let id = rule.id.clone();
        approve.connect_clicked(move |button| {
            automations::approve(&id);
            toast_from(button, "The rule may now act on this computer");
        });
        actions.append(&approve);
    }
    if !rule.is_builtin() {
        let edit_button = icon_button("document-edit-symbolic", "Edit", &["soft-button"]);
        let (win_weak, editing) = (win.downgrade(), rule.clone());
        edit_button.connect_clicked(move |_| {
            if let Some(win) = win_weak.upgrade() {
                edit(&win, editing.clone());
            }
        });
        actions.append(&edit_button);
        let delete = icon_button("user-trash-symbolic", "Delete", &["soft-button"]);
        let (id, label) = (rule.id.clone(), rule.label());
        delete.connect_clicked(move |button| {
            let id = id.clone();
            super::confirm(
                button,
                "Delete this rule?",
                &format!("“{label}” stops here and on your phone."),
                "Delete",
                move || automations::delete_rule(&id),
            );
        });
        actions.append(&delete);
    }
    row.add_row(&actions);

    let runs = automations::history(&rule.id);
    if runs.is_empty() {
        let none = text("Not run yet", &["text-faint", "body-small", "padded-row"]);
        row.add_row(&none);
    }
    for run in runs {
        row.add_row(&run_row(&run));
    }
    row
}

fn run_row(run: &Run) -> adw::ActionRow {
    let when = glib::DateTime::from_unix_local(run.at / 1000)
        .ok()
        .and_then(|time| time.format("%b %e, %H:%M").ok())
        .map(|time| time.to_string())
        .unwrap_or_default();
    let row = adw::ActionRow::builder()
        .title(format!("{when} · {}", run.cause))
        .subtitle(run.results.join("\n"))
        .use_markup(false)
        .subtitle_lines(4)
        .build();
    row.add_prefix(
        &gtk::Image::builder()
            .icon_name(if run.ok {
                "object-select-symbolic"
            } else {
                "dialog-warning-symbolic"
            })
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "{} run at {when}, {}",
        if run.ok { "Successful" } else { "Failed" },
        run.cause
    ))]);
    row
}

// ── The editor ─────────────────────────────────────────────────────────────

/// Reads a step's params back from its widgets.
type Reader = Box<dyn Fn(&mut Map<String, Value>)>;

/// One "When", "If" or "Do" step: its kind and param rows in a group.
struct StepEditor {
    catalog: &'static [Kind],
    group: adw::PreferencesGroup,
    kind: adw::ComboRow,
    rows: RefCell<Vec<gtk::Widget>>,
    readers: RefCell<Vec<Reader>>,
}

impl StepEditor {
    fn new(catalog: &'static [Kind], title: &str, step: &Step) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder().title(title).build();
        let labels: Vec<&str> = catalog.iter().map(|kind| kind.label).collect();
        let kind = adw::ComboRow::builder()
            .title("Kind")
            .model(&gtk::StringList::new(&labels))
            .build();
        let selected = catalog
            .iter()
            .position(|kind| kind.id == step.kind)
            .unwrap_or(0);
        kind.set_selected(selected as u32);
        group.add(&kind);
        let editor = Rc::new(Self {
            catalog,
            group,
            kind,
            rows: RefCell::new(Vec::new()),
            readers: RefCell::new(Vec::new()),
        });
        editor.show_params(step);
        let weak = Rc::downgrade(&editor);
        editor.kind.connect_selected_notify(move |_| {
            if let Some(editor) = weak.upgrade() {
                let kind = editor.chosen();
                editor.show_params(&Step::new(kind.id));
            }
        });
        editor
    }

    fn chosen(&self) -> &'static Kind {
        let index = (self.kind.selected() as usize).min(self.catalog.len() - 1);
        &self.catalog[index]
    }

    fn show_params(&self, step: &Step) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        self.readers.borrow_mut().clear();
        for (key, label, param) in self.chosen().params {
            let (row, reader) = param_row(key, label, *param, step.params.get(*key));
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
            self.readers.borrow_mut().push(reader);
        }
    }

    fn step(&self) -> Step {
        let mut step = Step::new(self.chosen().id);
        for reader in self.readers.borrow().iter() {
            reader(&mut step.params);
        }
        step
    }
}

/// A row for one param, and how to read it back.
fn param_row(
    key: &'static str,
    label: &str,
    param: Param,
    value: Option<&Value>,
) -> (gtk::Widget, Reader) {
    let text_value = value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match param {
        Param::Text(_) | Param::Time => {
            let row = adw::EntryRow::builder()
                .title(label)
                .text(&text_value)
                .build();
            if param == Param::Time {
                row.set_input_purpose(gtk::InputPurpose::Digits);
                if text_value.is_empty() {
                    row.set_text("08:00");
                }
            }
            let reader_row = row.clone();
            let reader: Reader = Box::new(move |params| {
                let text = reader_row.text().trim().to_string();
                if !text.is_empty() || param == Param::Time {
                    params.insert(key.into(), text.into());
                }
            });
            (row.upcast(), reader)
        }
        Param::Days => {
            let row = adw::ActionRow::builder().title(label).build();
            let days = gtk::Box::builder()
                .spacing(4)
                .valign(gtk::Align::Center)
                .css_classes(["linked"])
                .build();
            let mut toggles = Vec::new();
            for (index, name) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
                let digit = char::from(b'1' + index as u8);
                let full = [
                    "Monday",
                    "Tuesday",
                    "Wednesday",
                    "Thursday",
                    "Friday",
                    "Saturday",
                    "Sunday",
                ][index];
                let toggle = gtk::ToggleButton::builder()
                    .label(*name)
                    .tooltip_text(full)
                    .active(text_value.is_empty() || text_value.contains(digit))
                    .build();
                toggle.update_property(&[gtk::accessible::Property::Label(full)]);
                days.append(&toggle);
                toggles.push((digit, toggle));
            }
            row.add_suffix(&days);
            let reader: Reader = Box::new(move |params| {
                let chosen: String = toggles
                    .iter()
                    .filter(|(_, toggle)| toggle.is_active())
                    .map(|(digit, _)| *digit)
                    .collect();
                // Every day is the same as none chosen.
                if chosen.len() < 7 {
                    params.insert(key.into(), chosen.into());
                }
            });
            (row.upcast(), reader)
        }
        Param::Number(low, high) => {
            let row = adw::SpinRow::with_range(low as f64, high as f64, 1.0);
            row.set_title(label);
            row.set_value(value.and_then(Value::as_i64).unwrap_or(high.min(20)) as f64);
            let reader_row = row.clone();
            let reader: Reader = Box::new(move |params| {
                params.insert(key.into(), (reader_row.value() as i64).into());
            });
            (row.upcast(), reader)
        }
        Param::Bool => {
            let row = adw::SwitchRow::builder()
                .title(label)
                .active(value.and_then(Value::as_bool).unwrap_or(true))
                .build();
            let reader_row = row.clone();
            let reader: Reader = Box::new(move |params| {
                params.insert(key.into(), reader_row.is_active().into());
            });
            (row.upcast(), reader)
        }
        Param::Choice(choices) => {
            let labels: Vec<&str> = choices.iter().map(|(_, words)| *words).collect();
            let row = adw::ComboRow::builder()
                .title(label)
                .model(&gtk::StringList::new(&labels))
                .build();
            let index = choices
                .iter()
                .position(|(id, _)| *id == text_value)
                .unwrap_or(0);
            row.set_selected(index as u32);
            let reader_row = row.clone();
            let reader: Reader = Box::new(move |params| {
                let index = (reader_row.selected() as usize).min(choices.len() - 1);
                params.insert(key.into(), choices[index].0.into());
            });
            (row.upcast(), reader)
        }
        Param::Command => {
            let commands = crate::device_links::config::app_settings().run_commands;
            if commands.is_empty() {
                let row = adw::ActionRow::builder()
                    .title(label)
                    .subtitle("Add a command in Preferences → Run Commands first")
                    .build();
                return (row.upcast(), Box::new(|_| {}));
            }
            let names: Vec<&str> = commands
                .iter()
                .map(|command| command.name.as_str())
                .collect();
            let row = adw::ComboRow::builder()
                .title(label)
                .model(&gtk::StringList::new(&names))
                .build();
            let index = commands
                .iter()
                .position(|command| command.id == text_value)
                .unwrap_or(0);
            row.set_selected(index as u32);
            let ids: Vec<String> = commands.into_iter().map(|command| command.id).collect();
            let reader_row = row.clone();
            let reader: Reader = Box::new(move |params| {
                if let Some(id) = ids.get(reader_row.selected() as usize) {
                    params.insert(key.into(), id.clone().into());
                }
            });
            (row.upcast(), reader)
        }
    }
}

/// A list of steps ("If", "Do"), each removable, with Add.
struct StepList {
    catalog: &'static [Kind],
    title: &'static str,
    box_: gtk::Box,
    add: gtk::Button,
    items: RefCell<Vec<(Rc<StepEditor>, gtk::Widget)>>,
    max: usize,
}

impl StepList {
    fn new(
        catalog: &'static [Kind],
        title: &'static str,
        add_label: &str,
        steps: &[Step],
        max: usize,
    ) -> Rc<Self> {
        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .build();
        let add = icon_button("list-add-symbolic", add_label, &["soft-button"]);
        add.set_halign(gtk::Align::Start);
        let list = Rc::new(Self {
            catalog,
            title,
            box_,
            add,
            items: RefCell::new(Vec::new()),
            max,
        });
        list.box_.append(&list.add);
        for step in steps {
            list.push(step);
        }
        let weak = Rc::downgrade(&list);
        list.add.connect_clicked(move |_| {
            if let Some(list) = weak.upgrade() {
                list.push(&Step::new(list.catalog[0].id));
            }
        });
        list
    }

    fn push(self: &Rc<Self>, step: &Step) {
        if self.items.borrow().len() >= self.max {
            return;
        }
        let number = self.items.borrow().len() + 1;
        let editor = StepEditor::new(self.catalog, &format!("{} {number}", self.title), step);
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        remove.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove {} {number}",
            self.title
        ))]);
        editor.group.set_header_suffix(Some(&remove));
        let widget: gtk::Widget = editor.group.clone().upcast();
        self.box_.insert_child_after(
            &widget,
            self.items.borrow().last().map(|(_, widget)| widget),
        );
        self.items.borrow_mut().push((editor, widget.clone()));
        let weak = Rc::downgrade(self);
        remove.connect_clicked(move |_| {
            if let Some(list) = weak.upgrade() {
                list.box_.remove(&widget);
                list.items
                    .borrow_mut()
                    .retain(|(_, known)| known != &widget);
                list.add.set_visible(true);
            }
        });
        self.add.set_visible(self.items.borrow().len() < self.max);
    }

    fn steps(&self) -> Vec<Step> {
        self.items
            .borrow()
            .iter()
            .map(|(editor, _)| editor.step())
            .collect()
    }
}

/// Opens the editor on `rule` (new when its id is empty).
pub(super) fn edit(win: &DesklinkWindow, rule: Rule) {
    let page = adw::PreferencesPage::new();
    let about = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder()
        .title("Name (optional)")
        .text(&rule.name)
        .build();
    about.add(&name);
    let place = adw::ComboRow::builder()
        .title("Runs on")
        .subtitle("Where the rule watches for its trigger")
        .model(&gtk::StringList::new(&["This computer", "The phone"]))
        .selected(u32::from(rule.run_on == Side::Phone))
        .build();
    about.add(&place);
    page.add(&about);

    let when = StepEditor::new(TRIGGERS, "When", &rule.trigger);
    page.add(&when.group);

    let lists = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let ifs = StepList::new(CONDITIONS, "If", "Add a Condition", &rule.conditions, 8);
    let dos = StepList::new(ACTIONS, "Do", "Add an Action", &rule.actions, 8);
    if rule.actions.is_empty() {
        dos.push(&Step::new("lock-desk"));
    }
    lists.append(&ifs.box_);
    lists.append(&dos.box_);
    let holder = adw::PreferencesGroup::new();
    holder.add(&lists);
    page.add(&holder);

    let cancel = gtk::Button::builder().label("Cancel").build();
    let save = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    // Why Save refused, above the form (a toast would sit under the dialog).
    let problem = adw::Banner::new("");
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&problem);
    view.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(if rule.id.is_empty() {
            "New Rule"
        } else {
            "Edit Rule"
        })
        .content_width(560)
        .content_height(720)
        .child(&view)
        .build();
    {
        let dialog = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        });
    }
    {
        let dialog_weak = dialog.downgrade();
        save.connect_clicked(move |button| {
            let edited = Rule {
                name: name.text().to_string(),
                run_on: if place.selected() == 1 {
                    Side::Phone
                } else {
                    Side::Desk
                },
                trigger: when.step(),
                conditions: ifs.steps(),
                actions: dos.steps(),
                ..rule.clone()
            };
            match automations::save_rule(edited) {
                Ok(()) => {
                    toast_from(button, "Rule saved");
                    if let Some(dialog) = dialog_weak.upgrade() {
                        dialog.close();
                    }
                }
                Err(error) => {
                    problem.set_title(&error);
                    problem.set_revealed(true);
                }
            }
        });
    }
    dialog.present(Some(win));
}
