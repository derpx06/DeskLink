//! The tray indicator: the always-available surface on any desktop. Its menu
//! has everything the GNOME extension offers — each phone with Ring, Lock,
//! Send Files and Send Clipboard, Keep Awake, what is live here with End,
//! End Everything, Lock & Hand Off, recent files and the phone's
//! notifications — built by the application ([`TrayEntry`]).
//!
//! A StatusNotifierItem with a `com.canonical.dbusmenu` menu, registered with
//! `org.kde.StatusNotifierWatcher` — the protocol the GNOME AppIndicator /
//! tray extensions, KDE and most other desktops host. Written directly on
//! GDBus rather than pulling in a tray crate: two small interfaces, and it
//! runs on the GTK main loop like everything else here.
//!
//! The item owns `org.kde.StatusNotifierItem-<pid>-1` and counts as shown
//! only while the watcher reports a registered host. Without one (stock GNOME
//! without a tray extension) the application falls back to a notification
//! with an "End session" button whenever a session starts.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::variant::ToVariant;
use gtk::glib::{self, Variant};

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The tray icon: DeskLink's link glyph — a phone connected, none, or a
/// phone using this computer (with a red dot).
pub const ICON_CONNECTED: &str = "desklink-tray-connected-symbolic";
pub const ICON_OFFLINE: &str = "desklink-tray-offline-symbolic";
pub const ICON_LIVE: &str = "desklink-tray-live-symbolic";

const GLYPH: &str = "M2.5 1H9.5A1.5 1.5 0 0 1 11 2.5V6.5A1.5 1.5 0 0 1 9.5 8H2.5A1.5 1.5 0 0 1 1 6.5V2.5A1.5 1.5 0 0 1 2.5 1ZM10.5 4H12.5A1.5 1.5 0 0 1 14 5.5V12.5A1.5 1.5 0 0 1 12.5 14H10.5A1.5 1.5 0 0 1 9 12.5V5.5A1.5 1.5 0 0 1 10.5 4ZM4.5 9H7.5A0.5 0.5 0 0 1 8 9.5V9.5A0.5 0.5 0 0 1 7.5 10H4.5A0.5 0.5 0 0 1 4 9.5V9.5A0.5 0.5 0 0 1 4.5 9Z";

/// The three icons, as symbolic SVG (a host recolours them; `error` is the
/// theme's red).
fn icon_svg(name: &str) -> String {
    let glyph = |opacity: &str| {
        format!(r##"<path d="{GLYPH}" fill="#2e3436" fill-rule="evenodd" opacity="{opacity}"/>"##)
    };
    let body = match name {
        ICON_OFFLINE => glyph("0.45"),
        ICON_LIVE => format!(
            r##"<mask id="m"><rect width="16" height="16" fill="#fff"/><circle cx="13" cy="3" r="3.5" fill="#000"/></mask><g mask="url(#m)">{}</g><circle class="error" cx="13" cy="3" r="2.5" fill="#e5533d"/>"##,
            glyph("1")
        ),
        _ => glyph("1"),
    };
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">{body}</svg>"#
    )
}

/// Writes the tray icons into the user's own icon theme
/// (`$XDG_DATA_HOME/icons/hicolor/scalable/apps`) and returns that theme
/// root for `IconThemePath`. A host already searching it (GNOME's
/// AppIndicator) then looks the name up like any themed symbolic icon, so it
/// is recoloured for the panel; a private path was drawn as a plain picture.
fn install_icons() -> Option<std::path::PathBuf> {
    let root = glib::user_data_dir().join("icons");
    let dir = root.join("hicolor").join("scalable").join("apps");
    std::fs::create_dir_all(&dir).ok()?;
    for name in [ICON_CONNECTED, ICON_OFFLINE, ICON_LIVE] {
        let svg = icon_svg(name);
        let path = dir.join(format!("{name}.svg"));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(&path, &svg).ok()?;
        }
    }
    Some(root)
}

const XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
    <signal name="NewIcon"/>
    <signal name="NewAttentionIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus"><arg type="s"/></signal>
  </interface>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/>
      <arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" direction="in"/><arg type="as" direction="in"/>
      <arg type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg type="a(isvu)" direction="in"/><arg type="ai" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg type="i" direction="in"/><arg type="b" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg type="ai" direction="in"/><arg type="ai" direction="out"/><arg type="ai" direction="out"/>
    </method>
    <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
  </interface>
</node>"#;

/// What the menu acts on. Called on the main loop.
pub struct TrayActions {
    /// The item itself was clicked.
    pub open: Box<dyn Fn()>,
    /// A menu entry was chosen: its action name (see [`TrayEntry`]).
    pub run: Box<dyn Fn(&str)>,
}

/// One menu entry. `icon` is an icon name ("" none).
#[derive(Debug, Clone, PartialEq)]
pub enum TrayEntry {
    /// Words only, greyed.
    Label(String),
    /// A section's heading: greyed, with an icon.
    Header {
        label: String,
        icon: String,
    },
    Separator,
    /// Runs `action` (an application-defined name, e.g. `ring:<device>`).
    Action {
        label: String,
        action: String,
        enabled: bool,
        icon: String,
    },
    /// A check item.
    Toggle {
        label: String,
        action: String,
        on: bool,
        icon: String,
    },
    Submenu {
        label: String,
        children: Vec<TrayEntry>,
        icon: String,
    },
}

impl TrayEntry {
    pub fn action(label: impl Into<String>, action: impl Into<String>) -> Self {
        Self::Action {
            label: label.into(),
            action: action.into(),
            enabled: true,
            icon: String::new(),
        }
    }

    /// The same entry with `name` as its icon.
    pub fn icon(mut self, name: &str) -> Self {
        match &mut self {
            Self::Header { icon, .. }
            | Self::Action { icon, .. }
            | Self::Toggle { icon, .. }
            | Self::Submenu { icon, .. } => *icon = name.to_string(),
            Self::Label(_) | Self::Separator => {}
        }
        self
    }
}

#[derive(Default)]
struct State {
    /// One line per active session, e.g. "Pixel 8 — controlling the screen".
    lines: Vec<String>,
    entries: Vec<TrayEntry>,
    revision: u32,
    /// One of the `ICON_*` names.
    icon: &'static str,
    /// Where the icons were written.
    theme_path: String,
    /// Another DeskLink surface (the GNOME extension) shows the same:
    /// hosts hide a passive item, so the top bar has one DeskLink, not two.
    passive: bool,
}

pub struct Tray {
    connection: gio::DBusConnection,
    state: Rc<RefCell<State>>,
    /// Whether a host is showing us; the fallback notification is used if not.
    hosted: Hosted,
}

/// Registered with the watcher, and the watcher has at least one host: only
/// then is the item actually on screen somewhere.
#[derive(Clone, Default)]
struct Hosted {
    registered: Rc<Cell<bool>>,
    host: Rc<Cell<bool>>,
}

impl Tray {
    /// Exports the item and registers it with the watcher whenever one
    /// appears (including after the shell restarts).
    pub fn new(actions: TrayActions) -> Result<Self, String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|error| error.to_string())?;
        let node = gio::DBusNodeInfo::for_xml(XML).map_err(|error| error.to_string())?;
        let state = Rc::new(RefCell::new(State {
            icon: ICON_OFFLINE,
            theme_path: install_icons()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            ..State::default()
        }));
        let actions = Rc::new(actions);

        let item = node
            .lookup_interface(ITEM_INTERFACE)
            .ok_or("tray interface missing")?;
        let item_state = Rc::clone(&state);
        let item_actions = Rc::clone(&actions);
        connection
            .register_object(ITEM_PATH, &item)
            .method_call(move |_, _, _, _, method, _, invocation| {
                crate::guard("tray call", (), || {
                    if matches!(method, "Activate" | "SecondaryActivate") {
                        (item_actions.open)();
                    }
                    invocation.return_value(None);
                })
            })
            .property(move |_, _, _, _, name| item_property(name, &item_state.borrow()))
            .build()
            .map_err(|error| error.to_string())?;

        let menu = node
            .lookup_interface(MENU_INTERFACE)
            .ok_or("tray menu interface missing")?;
        let menu_state = Rc::clone(&state);
        connection
            .register_object(MENU_PATH, &menu)
            .method_call(move |_, _, _, _, method, params, invocation| {
                crate::guard("tray menu call", (), || {
                    let reply = menu_call(method, &params, &menu_state.borrow(), &actions);
                    invocation.return_value(reply.as_ref());
                })
            })
            .property(|_, _, _, _, name| match name {
                "Version" => 3u32.to_variant(),
                "TextDirection" => "ltr".to_variant(),
                "Status" => "normal".to_variant(),
                _ => Vec::<String>::new().to_variant(),
            })
            .build()
            .map_err(|error| error.to_string())?;

        // Hosts look for items under this name; the object paths stay as they are.
        let service = item_bus_name(std::process::id());
        let _owner = gio::bus_own_name_on_connection(
            &connection,
            &service,
            gio::BusNameOwnerFlags::NONE,
            |_, _| {},
            |_, name| debug_log!("[Tray] Could not own {name}; registering by unique name"),
        );

        let hosted = Hosted::default();
        // Any change in which hosts exist: ask the watcher again rather than
        // trusting the signal's sender or contents.
        for (interface, arg0) in [(WATCHER, None), (PROPERTIES, Some(WATCHER))] {
            let hosted = hosted.clone();
            connection.signal_subscribe(
                None,
                Some(interface),
                None,
                Some(WATCHER_PATH),
                arg0,
                gio::DBusSignalFlags::NONE,
                move |connection, _, _, _, member, _| {
                    crate::guard("tray signal", (), || {
                        if member != "StatusNotifierItemRegistered"
                            && member != "StatusNotifierItemUnregistered"
                        {
                            refresh_host(connection, &hosted);
                        }
                    })
                },
            );
        }

        let appeared = hosted.clone();
        let vanished = hosted.clone();
        // Watching lasts as long as the process; the tray is never torn down.
        let _watch = gio::bus_watch_name_on_connection(
            &connection,
            WATCHER,
            gio::BusNameWatcherFlags::NONE,
            move |connection, _, _| {
                register(&connection, &service, &appeared);
                refresh_host(&connection, &appeared);
            },
            move |_, _| {
                vanished.registered.set(false);
                vanished.host.set(false);
            },
        );

        Ok(Self {
            connection,
            state,
            hosted,
        })
    }

    /// Whether a tray host is showing the item.
    pub fn hosted(&self) -> bool {
        self.hosted.registered.get() && self.hosted.host.get()
    }

    /// Shows `lines` (one per active session; the tooltip), the menu
    /// `entries` and the `icon` (an `ICON_*`). Signals only on change.
    pub fn update(
        &self,
        lines: Vec<String>,
        entries: Vec<TrayEntry>,
        icon: &'static str,
        passive: bool,
    ) {
        let (was_status, icon_changed);
        {
            let mut state = self.state.borrow_mut();
            if state.lines == lines
                && state.entries == entries
                && state.icon == icon
                && state.passive == passive
            {
                return;
            }
            was_status = status(&state);
            state.passive = passive;
            icon_changed = state.icon != icon;
            state.lines = lines;
            state.entries = entries;
            state.icon = icon;
            state.revision = state.revision.wrapping_add(1);
        }
        let state = self.state.borrow();
        let emit = |path: &str, interface: &str, signal: &str, args: Option<Variant>| {
            let _ = self
                .connection
                .emit_signal(None, path, interface, signal, args.as_ref());
        };
        emit(
            MENU_PATH,
            MENU_INTERFACE,
            "LayoutUpdated",
            Some((state.revision, 0i32).to_variant()),
        );
        emit(ITEM_PATH, ITEM_INTERFACE, "NewToolTip", None);
        if icon_changed {
            emit(ITEM_PATH, ITEM_INTERFACE, "NewIcon", None);
            // Hosts re-read the attention icon only on its own signal.
            emit(ITEM_PATH, ITEM_INTERFACE, "NewAttentionIcon", None);
        }
        if was_status != status(&state) {
            emit(
                ITEM_PATH,
                ITEM_INTERFACE,
                "NewStatus",
                Some((status(&state),).to_variant()),
            );
        }
    }
}

/// `org.kde.StatusNotifierItem-<pid>-<n>`, the name hosts expect an item under.
fn item_bus_name(pid: u32) -> String {
    format!("{ITEM_INTERFACE}-{pid}-1")
}

/// Registers with the watcher by well-known name, falling back to the unique
/// name when that was refused (e.g. a sandbox that may not own it).
fn register(connection: &gio::DBusConnection, service: &str, hosted: &Hosted) {
    let registered = hosted.registered.clone();
    let retry = connection.clone();
    register_as(connection, service, move |ok| {
        let unique = retry.unique_name();
        match unique {
            Some(unique) if !ok => {
                register_as(&retry, &unique, move |ok| registered.set(ok));
            }
            _ => registered.set(ok),
        }
    });
}

fn register_as(connection: &gio::DBusConnection, service: &str, done: impl FnOnce(bool) + 'static) {
    connection.call(
        Some(WATCHER),
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        Some(&(service,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        2000,
        None::<&gio::Cancellable>,
        move |reply| done(reply.is_ok()),
    );
}

/// Reads `IsStatusNotifierHostRegistered` from the watcher.
fn refresh_host(connection: &gio::DBusConnection, hosted: &Hosted) {
    let host = hosted.host.clone();
    connection.call(
        Some(WATCHER),
        WATCHER_PATH,
        PROPERTIES,
        "Get",
        Some(&(WATCHER, "IsStatusNotifierHostRegistered").to_variant()),
        Some(glib::VariantTy::new("(v)").expect("valid type")),
        gio::DBusCallFlags::NONE,
        2000,
        None::<&gio::Cancellable>,
        move |reply| host.set(reply.ok().as_ref().is_some_and(host_registered)),
    );
}

/// The `(v)` reply of `Properties.Get` for the host flag.
fn host_registered(reply: &Variant) -> bool {
    reply
        .try_child_value(0)
        .and_then(|value| value.as_variant())
        .and_then(|value| value.get::<bool>())
        .unwrap_or(false)
}

fn status(state: &State) -> &'static str {
    if state.passive {
        "Passive"
    } else if state.lines.is_empty() {
        "Active"
    } else {
        "NeedsAttention"
    }
}

fn item_property(name: &str, state: &State) -> Variant {
    match name {
        "Category" => "Communications".to_variant(),
        "Id" => "desklink".to_variant(),
        "Title" => "DeskLink".to_variant(),
        "Status" => status(state).to_variant(),
        // Hosts show this one while `NeedsAttention` (a live session);
        // without it GNOME's AppIndicator drew a placeholder.
        "IconName" | "AttentionIconName" => state.icon.to_variant(),
        "IconThemePath" => state.theme_path.to_variant(),
        "ToolTip" => {
            let body = if state.lines.is_empty() {
                "No phone is using this computer".to_string()
            } else {
                state.lines.join("\n")
            };
            (
                state.icon,
                Vec::<(i32, i32, Vec<u8>)>::new(),
                "DeskLink",
                body,
            )
                .to_variant()
        }
        "ItemIsMenu" => true.to_variant(),
        "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH.to_string())
            .map(|path| path.to_variant())
            .unwrap_or_else(|_| "/".to_variant()),
        _ => "".to_variant(),
    }
}

/// The menu, numbered depth-first from 1: each entry's id, properties,
/// action (if any) and children's ids.
struct Numbered {
    id: i32,
    props: HashMap<String, Variant>,
    action: Option<String>,
    children: Vec<Numbered>,
}

/// A label as dbusmenu shows it: `_` marks a mnemonic there, so a file
/// name's underscores are doubled to stay visible.
fn label_text(label: &str) -> Variant {
    label.replace('_', "__").to_variant()
}

/// An id from what the entry is and where: hosts (GNOME's AppIndicator
/// extension) cache properties by id and re-read only the layout on
/// `LayoutUpdated`, so an id reused for a different entry showed the old
/// one's check or separator. And an id that moved made AppIndicator (on
/// GNOME 46+) drop the item (`menu.box.add` is gone), so a changed menu only
/// ever adds and removes. Equal ones are told apart.
fn stable_id(
    entry: &TrayEntry,
    parent: i32,
    index: usize,
    taken: &mut std::collections::HashSet<i32>,
) -> i32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (format!("{entry:?}"), parent, index).hash(&mut hasher);
    // Positive and not 0 (the root).
    let mut id = (hasher.finish() % 2_000_000_000) as i32 + 1;
    while !taken.insert(id) {
        id = id % 2_000_000_000 + 1;
    }
    id
}

fn number(
    entries: &[TrayEntry],
    parent: i32,
    taken: &mut std::collections::HashSet<i32>,
) -> Vec<Numbered> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let id = stable_id(entry, parent, index, taken);
            let mut props = HashMap::new();
            let mut action = None;
            let mut children = Vec::new();
            let icon = match entry {
                TrayEntry::Header { icon, .. }
                | TrayEntry::Action { icon, .. }
                | TrayEntry::Toggle { icon, .. }
                | TrayEntry::Submenu { icon, .. } => icon.as_str(),
                _ => "",
            };
            if !icon.is_empty() {
                props.insert("icon-name".to_string(), icon.to_variant());
            }
            match entry {
                TrayEntry::Label(label) | TrayEntry::Header { label, .. } => {
                    props.insert("label".to_string(), label_text(label));
                    props.insert("enabled".to_string(), false.to_variant());
                }
                TrayEntry::Separator => {
                    props.insert("type".to_string(), "separator".to_variant());
                }
                TrayEntry::Action {
                    label,
                    action: name,
                    enabled,
                    ..
                } => {
                    props.insert("label".to_string(), label_text(label));
                    if !enabled {
                        props.insert("enabled".to_string(), false.to_variant());
                    }
                    action = Some(name.clone());
                }
                TrayEntry::Toggle {
                    label,
                    action: name,
                    on,
                    ..
                } => {
                    props.insert("label".to_string(), label_text(label));
                    props.insert("toggle-type".to_string(), "checkmark".to_variant());
                    props.insert("toggle-state".to_string(), i32::from(*on).to_variant());
                    action = Some(name.clone());
                }
                TrayEntry::Submenu {
                    label,
                    children: inner,
                    ..
                } => {
                    props.insert("label".to_string(), label_text(label));
                    props.insert("children-display".to_string(), "submenu".to_variant());
                    children = number(inner, id, taken);
                }
            }
            Numbered {
                id,
                props,
                action,
                children,
            }
        })
        .collect()
}

fn numbered(state: &State) -> Vec<Numbered> {
    number(&state.entries, 0, &mut std::collections::HashSet::new())
}

fn flatten(items: &[Numbered]) -> Vec<&Numbered> {
    items
        .iter()
        .flat_map(|item| std::iter::once(item).chain(flatten(&item.children)))
        .collect()
}

fn node(item: &Numbered) -> Variant {
    let children: Vec<Variant> = item.children.iter().map(node).collect();
    (item.id, item.props.clone(), children).to_variant()
}

fn layout(state: &State) -> Variant {
    let children: Vec<Variant> = numbered(state).iter().map(node).collect();
    let mut root = HashMap::new();
    root.insert("children-display".to_string(), "submenu".to_variant());
    (0i32, root, children).to_variant()
}

fn menu_call(
    method: &str,
    params: &Variant,
    state: &State,
    actions: &TrayActions,
) -> Option<Variant> {
    match method {
        "GetLayout" => Some(Variant::tuple_from_iter([
            state.revision.to_variant(),
            layout(state),
        ])),
        "GetGroupProperties" => {
            let wanted: Vec<i32> = params
                .try_child_value(0)
                .and_then(|ids| ids.get())
                .unwrap_or_default();
            let items = numbered(state);
            let found: Vec<(i32, HashMap<String, Variant>)> = flatten(&items)
                .into_iter()
                .filter(|item| wanted.is_empty() || wanted.contains(&item.id))
                .map(|item| (item.id, item.props.clone()))
                .collect();
            Some((found,).to_variant())
        }
        "GetProperty" => Some(("".to_variant(),).to_variant()),
        "Event" => {
            let id: i32 = params
                .try_child_value(0)
                .and_then(|id| id.get())
                .unwrap_or_default();
            let event: String = params
                .try_child_value(1)
                .and_then(|event| event.get())
                .unwrap_or_default();
            if event == "clicked" {
                let items = numbered(state);
                if let Some(action) = flatten(&items)
                    .into_iter()
                    .find(|item| item.id == id)
                    .and_then(|item| item.action.clone())
                {
                    (actions.run)(&action);
                }
            }
            None
        }
        "EventGroup" => Some((Vec::<i32>::new(),).to_variant()),
        "AboutToShow" => Some((false,).to_variant()),
        "AboutToShowGroup" => Some((Vec::<i32>::new(), Vec::<i32>::new()).to_variant()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_has_the_dbusmenu_shape_and_clicks_run_their_action() {
        let ran = Rc::new(RefCell::new(Vec::<String>::new()));
        let actions = TrayActions {
            open: Box::new(|| {}),
            run: Box::new({
                let ran = Rc::clone(&ran);
                move |action| ran.borrow_mut().push(action.to_string())
            }),
        };
        let state = State {
            lines: vec!["Pixel — controlling the screen".into()],
            entries: vec![
                TrayEntry::Label("Pixel · Connected".into()),
                TrayEntry::action("Ring Pixel", "ring:p1"),
                TrayEntry::Toggle {
                    label: "Keep Awake".into(),
                    action: "keep-awake-off".into(),
                    on: true,
                    icon: String::new(),
                },
                TrayEntry::Separator,
                TrayEntry::Submenu {
                    label: "Recent Files".into(),
                    children: vec![TrayEntry::action("my_file.pdf", "open-file:/tmp/a.pdf")
                        .icon("x-office-document-symbolic")],
                    icon: String::new(),
                },
                TrayEntry::Action {
                    label: "End Everything".into(),
                    action: "end-sessions:".into(),
                    enabled: false,
                    icon: String::new(),
                },
            ],
            revision: 1,
            icon: ICON_LIVE,
            theme_path: "/tmp/icons".into(),
            passive: false,
        };
        assert_eq!(layout(&state).type_().as_str(), "(ia{sv}av)");
        let reply = menu_call("GetLayout", &().to_variant(), &state, &actions).unwrap();
        assert_eq!(reply.type_().as_str(), "(u(ia{sv}av))");
        let ids = (Vec::<i32>::new(), Vec::<String>::new()).to_variant();
        let reply = menu_call("GetGroupProperties", &ids, &state, &actions).unwrap();
        assert_eq!(reply.type_().as_str(), "(a(ia{sv}))");
        assert_eq!(
            reply.child_value(0).n_children(),
            7,
            "the submenu's child too"
        );

        // Ids are depth-first: the file inside the submenu is 6.
        let items = numbered(&state);
        let id_of = |action: &str| {
            flatten(&items)
                .into_iter()
                .find(|item| item.action.as_deref() == Some(action))
                .map(|item| item.id)
                .unwrap()
        };
        let label_id = flatten(&items)
            .into_iter()
            .find(|item| item.action.is_none() && item.props.contains_key("label"))
            .unwrap()
            .id;
        let click = |id: i32| {
            menu_call(
                "Event",
                &(id, "clicked", 0i32.to_variant(), 0u32).to_variant(),
                &state,
                &actions,
            )
        };
        click(id_of("ring:p1"));
        click(id_of("open-file:/tmp/a.pdf"));
        click(label_id); // a label: nothing
        assert_eq!(*ran.borrow(), ["ring:p1", "open-file:/tmp/a.pdf"]);
        let items = numbered(&state);
        let toggle = flatten(&items)
            .into_iter()
            .find(|item| item.id == id_of("keep-awake-off"))
            .unwrap();
        assert_eq!(toggle.props.get("toggle-state"), Some(&1i32.to_variant()));
        let file = flatten(&items)
            .into_iter()
            .find(|item| item.id == id_of("open-file:/tmp/a.pdf"))
            .unwrap();
        // Ids: unique, positive, and the same for the same menu.
        let ids: std::collections::HashSet<i32> =
            flatten(&items).into_iter().map(|item| item.id).collect();
        assert_eq!(ids.len(), 7);
        assert!(ids.iter().all(|id| *id > 0));
        let again = numbered(&state);
        let same: std::collections::HashSet<i32> =
            flatten(&again).into_iter().map(|item| item.id).collect();
        assert_eq!(same, ids);
        assert_eq!(
            file.props.get("label"),
            Some(&"my__file.pdf".to_variant()),
            "no mnemonic"
        );
        assert_eq!(
            file.props.get("icon-name"),
            Some(&"x-office-document-symbolic".to_variant())
        );

        assert_eq!(
            item_property("Status", &state).str(),
            Some("NeedsAttention")
        );
        assert_eq!(item_property("IconName", &state).str(), Some(ICON_LIVE));
        assert_eq!(
            item_property("AttentionIconName", &state).str(),
            Some(ICON_LIVE)
        );
        assert_eq!(
            item_property("IconThemePath", &state).str(),
            Some("/tmp/icons")
        );
        for name in [ICON_CONNECTED, ICON_OFFLINE, ICON_LIVE] {
            let svg = icon_svg(name);
            assert!(svg.starts_with("<svg") && svg.contains(GLYPH), "{name}");
        }
        assert!(
            icon_svg(ICON_LIVE).contains("class=\"error\""),
            "the red dot"
        );
        assert_eq!(
            item_property("ToolTip", &state).type_().as_str(),
            "(sa(iiay)ss)"
        );
    }

    #[test]
    fn the_item_is_named_by_pid_and_hosting_is_read_from_the_watcher_reply() {
        assert_eq!(item_bus_name(4242), "org.kde.StatusNotifierItem-4242-1");
        let reply = |value: Variant| Variant::tuple_from_iter([Variant::from_variant(&value)]);
        assert!(host_registered(&reply(true.to_variant())));
        assert!(!host_registered(&reply(false.to_variant())));
        assert!(!host_registered(&reply("yes".to_variant())));
        assert!(!host_registered(&().to_variant()));
    }
}
