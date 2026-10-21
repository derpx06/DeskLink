//! The top-bar indicator: what phones are doing here, and a way to stop them.
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

const IDLE_ICON: &str = "derx06.desklink.com-symbolic";
const ACTIVE_ICON: &str = "screen-shared-symbolic";

const ID_END: i32 = 1001;
const ID_OPEN: i32 = 1002;
const ID_QUIT: i32 = 1003;
const ID_HAND_OFF: i32 = 1004;
/// Session lines take ids from here up; they are labels, not actions.
const ID_FIRST_LINE: i32 = 1;

const XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
    <method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method>
    <signal name="NewIcon"/>
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
    pub open: Box<dyn Fn()>,
    pub end_sessions: Box<dyn Fn()>,
    /// Lock this computer and offer the newest terminal to the phones.
    pub lock_hand_off: Box<dyn Fn()>,
    pub quit: Box<dyn Fn()>,
}

#[derive(Default)]
struct State {
    /// One line per active session, e.g. "Pixel 8 — controlling the screen".
    lines: Vec<String>,
    revision: u32,
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
        let state = Rc::new(RefCell::new(State::default()));
        let actions = Rc::new(actions);

        let item = node
            .lookup_interface(ITEM_INTERFACE)
            .ok_or("tray interface missing")?;
        let item_state = Rc::clone(&state);
        let item_actions = Rc::clone(&actions);
        connection
            .register_object(ITEM_PATH, &item)
            .method_call(move |_, _, _, _, method, _, invocation| {
                if matches!(method, "Activate" | "SecondaryActivate") {
                    (item_actions.open)();
                }
                invocation.return_value(None);
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
                let reply = menu_call(method, &params, &menu_state.borrow(), &actions);
                invocation.return_value(reply.as_ref());
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
                    if member != "StatusNotifierItemRegistered"
                        && member != "StatusNotifierItemUnregistered"
                    {
                        refresh_host(connection, &hosted);
                    }
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

    /// Shows `lines` (one per active session). Signals only on change.
    pub fn update(&self, lines: Vec<String>) {
        let was_active;
        {
            let mut state = self.state.borrow_mut();
            if state.lines == lines {
                return;
            }
            was_active = !state.lines.is_empty();
            state.lines = lines;
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
        if was_active == state.lines.is_empty() {
            emit(ITEM_PATH, ITEM_INTERFACE, "NewIcon", None);
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
    if state.lines.is_empty() {
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
        "IconName" => if state.lines.is_empty() {
            IDLE_ICON
        } else {
            ACTIVE_ICON
        }
        .to_variant(),
        "ToolTip" => {
            let body = if state.lines.is_empty() {
                "No phone is using this computer".to_string()
            } else {
                state.lines.join("\n")
            };
            (
                IDLE_ICON,
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

fn props(label: Option<&str>, enabled: bool, separator: bool) -> HashMap<String, Variant> {
    let mut props = HashMap::new();
    if separator {
        props.insert("type".to_string(), "separator".to_variant());
    }
    if let Some(label) = label {
        props.insert("label".to_string(), label.to_variant());
    }
    if !enabled {
        props.insert("enabled".to_string(), false.to_variant());
    }
    props
}

/// The menu as `(id, properties)` pairs, top to bottom.
fn items(state: &State) -> Vec<(i32, HashMap<String, Variant>)> {
    let mut items = Vec::new();
    if state.lines.is_empty() {
        items.push((
            ID_FIRST_LINE,
            props(Some("No phone is using this computer"), false, false),
        ));
    }
    for (index, line) in state.lines.iter().enumerate() {
        items.push((
            ID_FIRST_LINE + index as i32,
            props(Some(line), false, false),
        ));
    }
    items.push((900, props(None, true, true)));
    items.push((
        ID_END,
        props(
            Some("End All Phone Sessions"),
            !state.lines.is_empty(),
            false,
        ),
    ));
    items.push((ID_HAND_OFF, props(Some("Lock & Hand Off"), true, false)));
    items.push((ID_OPEN, props(Some("Open DeskLink"), true, false)));
    items.push((901, props(None, true, true)));
    items.push((ID_QUIT, props(Some("Quit DeskLink"), true, false)));
    items
}

fn layout(state: &State) -> Variant {
    let children: Vec<Variant> = items(state)
        .into_iter()
        .map(|(id, props)| (id, props, Vec::<Variant>::new()).to_variant())
        .collect();
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
            let found: Vec<(i32, HashMap<String, Variant>)> = items(state)
                .into_iter()
                .filter(|(id, _)| wanted.is_empty() || wanted.contains(id))
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
                match id {
                    ID_END => (actions.end_sessions)(),
                    ID_HAND_OFF => (actions.lock_hand_off)(),
                    ID_OPEN => (actions.open)(),
                    ID_QUIT => (actions.quit)(),
                    _ => {}
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
    fn the_layout_has_the_dbusmenu_shape_and_end_is_disabled_when_idle() {
        let idle = State::default();
        assert_eq!(layout(&idle).type_().as_str(), "(ia{sv}av)");
        let actions = TrayActions {
            open: Box::new(|| {}),
            end_sessions: Box::new(|| {}),
            lock_hand_off: Box::new(|| {}),
            quit: Box::new(|| {}),
        };
        let reply = menu_call("GetLayout", &().to_variant(), &idle, &actions).unwrap();
        assert_eq!(reply.type_().as_str(), "(u(ia{sv}av))");
        let ids = (Vec::<i32>::new(), Vec::<String>::new()).to_variant();
        let reply = menu_call("GetGroupProperties", &ids, &idle, &actions).unwrap();
        assert_eq!(reply.type_().as_str(), "(a(ia{sv}))");
        let end = items(&idle)
            .into_iter()
            .find(|(id, _)| *id == ID_END)
            .unwrap();
        assert_eq!(end.1.get("enabled"), Some(&false.to_variant()));

        let busy = State {
            lines: vec!["Pixel — controlling the screen".into()],
            revision: 1,
        };
        let end = items(&busy)
            .into_iter()
            .find(|(id, _)| *id == ID_END)
            .unwrap();
        assert!(!end.1.contains_key("enabled"));
        assert_eq!(item_property("Status", &busy).str(), Some("NeedsAttention"));
        assert_eq!(item_property("IconName", &busy).str(), Some(ACTIVE_ICON));
        assert_eq!(
            item_property("ToolTip", &busy).type_().as_str(),
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
