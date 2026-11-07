//! The Terminals page's interactive terminal: a VTE widget running
//! `desklink --attach-terminal <id> --embedded`, the client a desk terminal
//! window runs. So the attach protocol, the size rules (it is a desk window:
//! it sets the size, and typing in it takes the size back from the phone) and
//! the flicker rules apply unchanged, with nothing new on the phone's wire.
//! `--embedded` only tells the local socket that this window's resizes follow
//! the page's layout and that its input may be VTE's own (focus reports,
//! answers to queries): neither takes the phone's size claim. A real key
//! press or paste does, through `pty::desk::desk_acted`.
//!
//! It is attached only while it is on screen: shown, it attaches once its
//! size has settled (so the attach carries the real size, not VTE's 80x24);
//! hidden (another page, the grid, the window closed to the background) it
//! detaches, so a window nobody looks at never holds the session's size. A
//! phone's claim survives both.
//!
//! Keys: while it has focus every Ctrl shortcut is the program's (Ctrl+K
//! kills a line, Ctrl+F moves a character, Ctrl+W deletes a word), so the
//! window's Ctrl shortcuts are handed to the terminal instead. Ctrl+Shift+C
//! copies, Ctrl+Shift+V pastes, Ctrl+Shift+K opens Find anything, and
//! Ctrl+Tab leaves the terminal.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use vte4::prelude::*;

use crate::device_links::pty::registry::SessionInfo;

/// What the session's terminal has to say once its attach client is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ended {
    /// The program in the session exited (`None`: it is already gone).
    Finished(Option<u32>),
    /// The session runs on, but this view's client stopped.
    Disconnected,
    /// The client could not be started at all.
    CouldNotStart,
}

impl Ended {
    /// The ended state's headline and its explanation.
    pub(crate) fn words(&self) -> (String, &'static str) {
        match self {
            Self::Finished(Some(status)) => (
                format!("Session ended (exit {status})"),
                "Restart starts a new shell in the same folder.",
            ),
            Self::Finished(None) => (
                "Session ended".to_string(),
                "Restart starts a new shell in the same folder.",
            ),
            Self::Disconnected => (
                "Disconnected from this terminal".to_string(),
                "It is still running. Restart connects to it again.",
            ),
            Self::CouldNotStart => (
                "Could not open the terminal here".to_string(),
                "Try Restart, or Open in terminal window.",
            ),
        }
    }
}

/// Why the client exited, from what the registry says of the session now.
pub(crate) fn ended(session: Option<&SessionInfo>) -> Ended {
    match session {
        None => Ended::Finished(None),
        Some(info) => match info.exit_status {
            Some(status) => Ended::Finished(Some(status)),
            None => Ended::Disconnected,
        },
    }
}

/// The command the view runs: this very program, attaching to `id` as the
/// window's own terminal. Inside a Flatpak too: the view runs in the
/// sandbox, beside the socket.
pub(crate) fn attach_argv(exe: &str, id: &str) -> Vec<String> {
    vec![
        exe.to_string(),
        "--attach-terminal".to_string(),
        id.to_string(),
        "--embedded".to_string(),
    ]
}

/// Whether the terminal's size has settled enough to attach: allocated and
/// the same grid two looks in a row, or waited long enough.
fn ready_to_attach(allocated: bool, grid: (i64, i64), last: (i64, i64), looks: u32) -> bool {
    (allocated && grid == last) || looks >= SETTLE_LOOKS
}

/// How often, and at most how many times, the size is looked at.
const SETTLE_EVERY: std::time::Duration = std::time::Duration::from_millis(30);
const SETTLE_LOOKS: u32 = 40;

/// A key press that is the person typing: not a lone modifier, and not
/// VTE's own scrollback keys (Shift+Page Up and the like), which never reach
/// the program.
fn is_typing(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    use gtk::gdk::Key;
    let modifier = matches!(
        keyval,
        Key::Shift_L
            | Key::Shift_R
            | Key::Control_L
            | Key::Control_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Super_L
            | Key::Super_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Caps_Lock
            | Key::Num_Lock
            | Key::ISO_Level3_Shift
    );
    let scrollback = state.contains(gtk::gdk::ModifierType::SHIFT_MASK)
        && matches!(
            keyval,
            Key::Page_Up
                | Key::Page_Down
                | Key::Home
                | Key::End
                | Key::KP_Page_Up
                | Key::KP_Page_Down
        );
    !modifier && !scrollback
}

/// The client's whole environment: only what it needs to find the socket
/// (`pty::host::runtime_dir`, the cache fallback). The shell it shows runs
/// in the session, with the session's own environment.
const PASSED: [&str; 4] = ["HOME", "XDG_RUNTIME_DIR", "XDG_CACHE_HOME", "FLATPAK_ID"];

pub(crate) fn attach_env(get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    PASSED
        .iter()
        .filter_map(|name| get(name).map(|value| format!("{name}={value}")))
        .collect()
}

/// VTE's "do not add the parent's environment" spawn flag
/// (`VTE_SPAWN_NO_PARENT_ENVV`), which the bindings do not name.
const NO_PARENT_ENVV: u32 = 1 << 25;

/// The DeskLink terminal colours: the phone's `TerminalPalette`.
const BACKGROUND: &str = "#0e0f11";
const FOREGROUND: &str = "#d9dbe0";
const CURSOR: &str = "#ff7a62";
const PALETTE: [&str; 16] = [
    "#2a2e37", "#ff6b6b", "#34d1a0", "#ffc24d", "#6e8cff", "#c394e8", "#5cc8d6", "#c7cbd3",
    "#5c6370", "#ff8787", "#5fe0b8", "#ffd47a", "#93a9ff", "#d2acf0", "#86d9e3", "#f2f3f5",
];

/// The system's monospace font (GNOME's setting), else "Monospace 11".
fn system_monospace() -> gtk::pango::FontDescription {
    let gnome = gtk::gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup("org.gnome.desktop.interface", true))
        .filter(|schema| schema.has_key("monospace-font-name"))
        .map(|_| {
            gtk::gio::Settings::new("org.gnome.desktop.interface")
                .string("monospace-font-name")
                .to_string()
        })
        .filter(|name| !name.trim().is_empty());
    gtk::pango::FontDescription::from_string(gnome.as_deref().unwrap_or("Monospace 11"))
}

fn rgba(hex: &str) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::parse(hex).unwrap_or(gtk::gdk::RGBA::BLACK)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Not attached (not on screen yet, or hidden).
    Idle,
    /// Spawning the client.
    Starting,
    /// The client runs with this pid.
    Running(i32),
    /// The client exited by itself: the ended state shows.
    Ended,
}

struct Inner {
    id: String,
    /// The session's folder, for Restart once it has ended.
    cwd: RefCell<Option<String>>,
    state: Cell<State>,
    /// Bumped by every attach and detach: a spawn finishing for an older one
    /// lets its client go.
    generation: Cell<u64>,
    terminal: vte4::Terminal,
    stack: gtk::Stack,
    headline: gtk::Label,
    explanation: gtk::Label,
}

/// One session's interactive terminal, with its ended state.
#[derive(Clone)]
pub(crate) struct TerminalView {
    inner: Rc<Inner>,
    pub(crate) root: gtk::Widget,
}

impl std::fmt::Debug for TerminalView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalView")
            .field("id", &self.inner.id)
            .field("state", &self.inner.state.get())
            .finish()
    }
}

impl TerminalView {
    pub(crate) fn new(info: &SessionInfo) -> Self {
        let terminal = vte4::Terminal::builder()
            .hexpand(true)
            .vexpand(true)
            .scrollback_lines(10_000)
            .cursor_blink_mode(vte4::CursorBlinkMode::System)
            .scroll_on_keystroke(true)
            .scroll_on_output(false)
            .allow_hyperlink(true)
            .build();
        terminal.set_widget_name(&format!("terminal-live:{}", info.id));
        terminal.add_css_class("term-live");
        let palette: Vec<gtk::gdk::RGBA> = PALETTE.iter().map(|hex| rgba(hex)).collect();
        terminal.set_colors(
            Some(&rgba(FOREGROUND)),
            Some(&rgba(BACKGROUND)),
            &palette.iter().collect::<Vec<_>>(),
        );
        terminal.set_color_cursor(Some(&rgba(CURSOR)));
        terminal.set_color_cursor_foreground(Some(&rgba(BACKGROUND)));
        terminal.set_font(Some(&system_monospace()));
        terminal.update_property(&[gtk::accessible::Property::Label("Terminal")]);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["term-live-scroll"])
            .child(&terminal)
            .build();

        // The ended state: what happened, Restart and Close.
        let headline = gtk::Label::builder()
            .css_classes(["terminal-title"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let explanation = gtk::Label::builder()
            .css_classes(["tile-meta"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let restart = gtk::Button::builder()
            .label("Restart")
            .tooltip_text("Restart this terminal")
            .css_classes(["dark-chip"])
            .build();
        restart.set_widget_name("terminal-restart");
        let close = gtk::Button::builder()
            .label("Close")
            .tooltip_text("Close this terminal")
            .css_classes(["dark-chip"])
            .build();
        close.set_widget_name("terminal-close");
        let buttons = gtk::Box::builder()
            .spacing(8)
            .halign(gtk::Align::Center)
            .build();
        buttons.append(&restart);
        buttons.append(&close);
        let ended_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .build();
        ended_box.set_widget_name("terminal-ended");
        ended_box.append(&headline);
        ended_box.append(&explanation);
        ended_box.append(&buttons);

        let stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
        stack.add_named(&scroll, Some("live"));
        stack.add_named(&ended_box, Some("ended"));

        let view = Self {
            inner: Rc::new(Inner {
                id: info.id.clone(),
                cwd: RefCell::new(info.cwd.clone()),
                state: Cell::new(State::Idle),
                generation: Cell::new(0),
                terminal: terminal.clone(),
                stack: stack.clone(),
                headline,
                explanation,
            }),
            root: stack.clone().upcast(),
        };
        view.add_menu();
        view.watch_the_person();

        let weak = Rc::downgrade(&view.inner);
        terminal.connect_child_exited(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                // Exits we caused (hidden, closed) are not an ending.
                if matches!(inner.state.get(), State::Running(_)) {
                    let id = inner.id.clone();
                    let now = crate::terminal_previews::sessions()
                        .into_iter()
                        .find(|info| info.id == id);
                    show_ended(&inner, &ended(now.as_ref()));
                }
            }
        });
        // On screen it attaches; off screen it lets go.
        let weak = Rc::downgrade(&view.inner);
        stack.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                if inner.state.get() == State::Idle {
                    schedule_attach(&inner);
                }
                // Shown is ready to type into.
                let weak = Rc::downgrade(&inner);
                gtk::glib::idle_add_local_once(move || {
                    if let Some(inner) = weak.upgrade() {
                        if inner.state.get() != State::Ended && inner.terminal.is_mapped() {
                            inner.terminal.grab_focus();
                        }
                    }
                });
            }
        });
        let weak = Rc::downgrade(&view.inner);
        stack.connect_unmap(move |_| {
            if let Some(inner) = weak.upgrade() {
                detach(&inner);
            }
        });
        let weak = Rc::downgrade(&view.inner);
        restart.connect_clicked(move |button| {
            if let Some(inner) = weak.upgrade() {
                restart_view(&inner, button);
            }
        });
        close.connect_clicked(super::terminals::close_detail_from);
        view
    }

    /// Keeps Restart's folder current.
    pub(crate) fn update(&self, info: &SessionInfo) {
        if info.cwd.is_some() {
            self.inner.cwd.replace(info.cwd.clone());
        }
    }

    /// Whether the ended state shows (the session may be gone).
    pub(crate) fn has_ended(&self) -> bool {
        self.inner.state.get() == State::Ended
    }

    /// The session disappeared while the client still ran: say so now.
    pub(crate) fn session_gone(&self) {
        if matches!(self.inner.state.get(), State::Running(_) | State::Starting) {
            detach(&self.inner);
            show_ended(&self.inner, &Ended::Finished(None));
        }
    }

    /// Types `text` as if at the keyboard (the QA scripts' hands).
    #[cfg(debug_assertions)]
    pub(crate) fn type_text(&self, text: &str) {
        self.inner.terminal.feed_child(text.as_bytes());
    }

    /// Lets go of the session for good (the detail closed).
    pub(crate) fn shut(&self) {
        detach(&self.inner);
    }

    /// The person typing or pasting here takes the size back from a phone
    /// (VTE's own replies and focus reports do not: see the module).
    fn watch_the_person(&self) {
        let terminal = &self.inner.terminal;
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&self.inner);
        keys.connect_key_pressed(move |_, keyval, _, state| {
            if let Some(inner) = weak.upgrade() {
                if is_typing(keyval, state) {
                    person_acted(&inner);
                }
            }
            gtk::glib::Propagation::Proceed
        });
        terminal.add_controller(keys);
        // The middle button pastes the selection.
        let middle = gtk::GestureClick::builder().button(2).build();
        middle.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&self.inner);
        middle.connect_pressed(move |_, _, _, _| {
            if let Some(inner) = weak.upgrade() {
                person_acted(&inner);
            }
        });
        terminal.add_controller(middle);
    }

    /// Copy, Paste and Select All, on the right button and the menu key.
    fn add_menu(&self) {
        let terminal = &self.inner.terminal;
        let group = gtk::gio::SimpleActionGroup::new();
        let copy = gtk::gio::SimpleAction::new("copy", None);
        let weak = terminal.downgrade();
        copy.connect_activate(move |_, _| {
            if let Some(terminal) = weak.upgrade() {
                terminal.copy_clipboard_format(vte4::Format::Text);
            }
        });
        let paste = gtk::gio::SimpleAction::new("paste", None);
        let weak = Rc::downgrade(&self.inner);
        paste.connect_activate(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                paste_into(&inner);
            }
        });
        let select_all = gtk::gio::SimpleAction::new("select-all", None);
        let weak = terminal.downgrade();
        select_all.connect_activate(move |_, _| {
            if let Some(terminal) = weak.upgrade() {
                terminal.select_all();
            }
        });
        group.add_action(&copy);
        group.add_action(&paste);
        group.add_action(&select_all);
        terminal.insert_action_group("term", Some(&group));
        let weak = copy.downgrade();
        terminal.connect_selection_changed(move |terminal| {
            if let Some(copy) = weak.upgrade() {
                copy.set_enabled(terminal.has_selection());
            }
        });
        copy.set_enabled(false);

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Copy"), Some("term.copy"));
        menu.append(Some("Paste"), Some("term.paste"));
        let all = gtk::gio::Menu::new();
        all.append(Some("Select all"), Some("term.select-all"));
        menu.append_section(None, &all);
        terminal.set_context_menu_model(Some(&menu));
    }
}

fn show_ended(inner: &Inner, why: &Ended) {
    inner.state.set(State::Ended);
    let (headline, explanation) = why.words();
    inner.headline.set_label(&headline);
    inner.explanation.set_label(explanation);
    inner.stack.set_visible_child_name("ended");
}

/// The person acted here: the size is this terminal's again.
fn person_acted(inner: &Inner) {
    if matches!(inner.state.get(), State::Running(_)) {
        let rows = u16::try_from(inner.terminal.row_count()).unwrap_or(24);
        let cols = u16::try_from(inner.terminal.column_count()).unwrap_or(80);
        crate::device_links::pty::desk::desk_acted(&inner.id, rows, cols);
    }
}

fn paste_into(inner: &Inner) {
    person_acted(inner);
    inner.terminal.paste_clipboard();
}

/// Attaches once the terminal has its size: `ATTACH` carries the grid the
/// client finds on its PTY, and VTE's is 80x24 until it is laid out.
fn schedule_attach(inner: &Rc<Inner>) {
    let generation = inner.generation.get() + 1;
    inner.generation.set(generation);
    inner.state.set(State::Starting);
    inner.stack.set_visible_child_name("live");
    let weak = Rc::downgrade(inner);
    let (last, looks) = (Cell::new((-1, -1)), Cell::new(0));
    gtk::glib::timeout_add_local(SETTLE_EVERY, move || {
        let Some(inner) = weak.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if inner.generation.get() != generation || inner.state.get() != State::Starting {
            return gtk::glib::ControlFlow::Break;
        }
        let terminal = &inner.terminal;
        let grid = (terminal.column_count(), terminal.row_count());
        looks.set(looks.get() + 1);
        let allocated = terminal.is_mapped() && terminal.width() > 0 && terminal.height() > 0;
        if ready_to_attach(allocated, grid, last.replace(grid), looks.get()) {
            attach(&inner, generation);
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

/// Starts the attach client in the terminal.
fn attach(inner: &Rc<Inner>, generation: u64) {
    let exe = crate::device_links::pty::host::own_executable(true);
    // A fresh screen: the client's first output is the session's snapshot.
    inner.terminal.reset(true, true);
    let argv = attach_argv(&exe.to_string_lossy(), &inner.id);
    let env = attach_env(|name| std::env::var(name).ok());
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let env: Vec<&str> = env.iter().map(String::as_str).collect();
    let weak = Rc::downgrade(inner);
    inner.terminal.spawn_async(
        vte4::PtyFlags::DEFAULT,
        None,
        &argv,
        &env,
        gtk::glib::SpawnFlags::from_bits_retain(NO_PARENT_ENVV),
        || {},
        -1,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let Some(inner) = weak.upgrade() else {
                if let Ok(pid) = result {
                    hang_up(pid.0);
                }
                return;
            };
            let current =
                inner.generation.get() == generation && inner.state.get() == State::Starting;
            match result {
                // Hidden (or attached again) before it started: let it go.
                Ok(pid) if !current => hang_up(pid.0),
                Ok(pid) => inner.state.set(State::Running(pid.0)),
                Err(_) if !current => {}
                Err(error) => {
                    eprintln!("[UI] Could not open the terminal here: {error}");
                    show_ended(&inner, &Ended::CouldNotStart);
                }
            }
        },
    );
}

/// Ends the attach client: the session runs on (and stops counting this as
/// a desk window).
fn detach(inner: &Inner) {
    inner.generation.set(inner.generation.get() + 1);
    if let State::Running(pid) = inner.state.get() {
        hang_up(pid);
    }
    if inner.state.get() != State::Ended {
        inner.state.set(State::Idle);
    }
}

fn hang_up(pid: i32) {
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGHUP) };
    }
}

/// Restart: attach again to a session that still runs, else a new shell in
/// the same folder, shown here.
fn restart_view(inner: &Rc<Inner>, button: &gtk::Button) {
    let id = inner.id.clone();
    let alive = crate::terminal_previews::sessions()
        .into_iter()
        .any(|info| info.id == id && info.exit_status.is_none());
    if alive {
        schedule_attach(inner);
        inner.terminal.grab_focus();
        return;
    }
    let folder = inner.cwd.borrow().clone();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let cwd = folder
        .zip(home)
        .and_then(|(folder, home)| crate::device_links::pty::registry::resolve_cwd(&folder, &home));
    let created = crate::device_links::pty::registry::registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned".to_string())
        .and_then(|mut registry| registry.create_in(None, 24, 80, "desk", cwd.as_deref(), None));
    match created {
        Ok(info) => super::terminals::open_detail_from(button, &info.id),
        Err(error) => {
            eprintln!("[UI] Could not restart the terminal: {error}");
            super::toast_from(button, "Could not start a new terminal");
        }
    }
}

/// While a terminal has focus, the window's Ctrl shortcuts are its keys.
/// Added to the window after the application's shortcuts, so it runs first
/// (GTK runs a widget's controllers newest first).
pub(crate) fn hand_keys_to_terminals(window: &impl IsA<gtk::Window>) {
    // ponytail: relies on GTK running a widget's controllers newest first
    // (`gtk_widget_add_controller` prepends); pinned by
    // `the_terminal_keys_run_before_the_windows_shortcuts`.
    let keys = gtk::EventControllerKey::new();
    keys.set_name(Some(TERMINAL_KEYS));
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let window_weak = window.as_ref().downgrade();
    keys.connect_key_pressed(move |keys, keyval, _, state| {
        let Some(window) = window_weak.upgrade() else {
            return gtk::glib::Propagation::Proceed;
        };
        let Some(terminal) = focused_terminal(&window) else {
            return gtk::glib::Propagation::Proceed;
        };
        match key_route(keyval, state) {
            KeyRoute::Normal => gtk::glib::Propagation::Proceed,
            KeyRoute::Copy => {
                if terminal.has_selection() {
                    terminal.copy_clipboard_format(vte4::Format::Text);
                }
                gtk::glib::Propagation::Stop
            }
            KeyRoute::Paste => {
                // Through the view's action: pasting is the person acting.
                let _ = terminal.activate_action("term.paste", None);
                gtk::glib::Propagation::Stop
            }
            KeyRoute::Palette => {
                let _ = window.activate_action("win.palette", None);
                gtk::glib::Propagation::Stop
            }
            KeyRoute::LeaveForward => {
                window.child_focus(gtk::DirectionType::TabForward);
                gtk::glib::Propagation::Stop
            }
            KeyRoute::LeaveBackward => {
                window.child_focus(gtk::DirectionType::TabBackward);
                gtk::glib::Propagation::Stop
            }
            KeyRoute::Terminal => {
                keys.forward(&terminal);
                gtk::glib::Propagation::Stop
            }
        }
    });
    window.as_ref().add_controller(keys);
}

/// The window's key controller for terminals, by name.
const TERMINAL_KEYS: &str = "desklink-terminal-keys";

/// The terminal that has the keyboard, if one does.
fn focused_terminal(window: &gtk::Window) -> Option<vte4::Terminal> {
    gtk::prelude::RootExt::focus(window).and_then(|focus| focus.downcast::<vte4::Terminal>().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    /// Not a shortcut: on its way to the terminal as usual.
    Normal,
    Copy,
    Paste,
    Palette,
    LeaveForward,
    LeaveBackward,
    /// A Ctrl key the window would take: the terminal's instead.
    Terminal,
}

pub(crate) fn key_route(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> KeyRoute {
    use gtk::gdk::{Key, ModifierType};
    let mods = state
        & (ModifierType::CONTROL_MASK
            | ModifierType::SHIFT_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SUPER_MASK);
    if !mods.contains(ModifierType::CONTROL_MASK) {
        return KeyRoute::Normal;
    }
    let shift = mods.contains(ModifierType::SHIFT_MASK);
    let ctrl_shift = ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK;
    match keyval.to_lower() {
        Key::ISO_Left_Tab => KeyRoute::LeaveBackward,
        Key::Tab | Key::KP_Tab if shift => KeyRoute::LeaveBackward,
        Key::Tab | Key::KP_Tab => KeyRoute::LeaveForward,
        Key::c if mods == ctrl_shift => KeyRoute::Copy,
        Key::v if mods == ctrl_shift => KeyRoute::Paste,
        Key::k if mods == ctrl_shift => KeyRoute::Palette,
        _ => KeyRoute::Terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk::{Key, ModifierType};

    fn info(exit_status: Option<u32>) -> SessionInfo {
        SessionInfo {
            id: "7".into(),
            title: "-bash".into(),
            rows: 37,
            cols: 99,
            exit_status,
            desk_window: false,
            phone_size: false,
            cwd: Some("~/api".into()),
            started_at: 0,
            shell: Some("bash".into()),
        }
    }

    /// What the view runs is what `main` reads as the window's own client,
    /// for this very session.
    #[test]
    fn the_view_runs_this_program_as_the_windows_own_client() {
        let argv = attach_argv("/usr/bin/desklink", "c5f37a76-73f4");
        assert_eq!(argv[0], "/usr/bin/desklink");
        assert_eq!(
            crate::device_links::pty::desk::attach_args(&argv),
            Some(("c5f37a76-73f4", true))
        );
    }

    /// It attaches once laid out with the same grid twice, or after waiting.
    #[test]
    fn it_attaches_only_once_its_size_has_settled() {
        // VTE's default 80x24 before the first allocation is not a size.
        assert!(!ready_to_attach(false, (80, 24), (80, 24), 2));
        assert!(!ready_to_attach(true, (53, 32), (80, 24), 3));
        assert!(ready_to_attach(true, (53, 32), (53, 32), 4));
        assert!(ready_to_attach(false, (80, 24), (-1, -1), SETTLE_LOOKS));
    }

    #[test]
    fn only_real_keys_count_as_the_person_typing() {
        let none = ModifierType::empty();
        let shift = ModifierType::SHIFT_MASK;
        assert!(is_typing(Key::a, none));
        assert!(is_typing(Key::Return, none));
        assert!(is_typing(Key::k, ModifierType::CONTROL_MASK));
        assert!(is_typing(Key::Page_Up, none), "the program gets it");
        assert!(!is_typing(Key::Control_L, ModifierType::CONTROL_MASK));
        assert!(!is_typing(Key::Shift_R, shift));
        assert!(
            !is_typing(Key::Page_Up, shift),
            "VTE scrolls; nothing is sent"
        );
    }

    #[test]
    fn the_client_gets_only_what_finds_the_socket() {
        let env = attach_env(|name| match name {
            "HOME" => Some("/home/sam".into()),
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
            "PATH" | "LD_PRELOAD" | "SHELL" => Some("leak".into()),
            _ => None,
        });
        assert_eq!(env, ["HOME=/home/sam", "XDG_RUNTIME_DIR=/run/user/1000"]);
    }

    #[test]
    fn an_ended_client_says_whether_the_session_ended_or_only_the_view() {
        assert_eq!(ended(None), Ended::Finished(None));
        assert_eq!(ended(Some(&info(Some(1)))), Ended::Finished(Some(1)));
        assert_eq!(ended(Some(&info(None))), Ended::Disconnected);
        assert_eq!(Ended::Finished(Some(1)).words().0, "Session ended (exit 1)");
        assert!(Ended::Disconnected.words().1.contains("still running"));
        // A client that never started says nothing about the session.
        assert!(!Ended::CouldNotStart.words().1.contains("running"));
    }

    #[test]
    fn ctrl_keys_are_the_terminals_but_for_copy_paste_and_the_palette() {
        let ctrl = ModifierType::CONTROL_MASK;
        let ctrl_shift = ctrl | ModifierType::SHIFT_MASK;
        for key in [
            Key::k,
            Key::f,
            Key::w,
            Key::n,
            Key::q,
            Key::_1,
            Key::_5,
            Key::comma,
        ] {
            assert_eq!(key_route(key, ctrl), KeyRoute::Terminal, "{key:?}");
        }
        assert_eq!(key_route(Key::C, ctrl_shift), KeyRoute::Copy);
        assert_eq!(key_route(Key::V, ctrl_shift), KeyRoute::Paste);
        assert_eq!(key_route(Key::K, ctrl_shift), KeyRoute::Palette);
        assert_eq!(key_route(Key::Tab, ctrl), KeyRoute::LeaveForward);
        assert_eq!(
            key_route(Key::ISO_Left_Tab, ctrl_shift),
            KeyRoute::LeaveBackward
        );
        assert_eq!(key_route(Key::k, ModifierType::empty()), KeyRoute::Normal);
        assert_eq!(
            key_route(Key::a, ModifierType::SHIFT_MASK),
            KeyRoute::Normal
        );
        // Caps Lock or Num Lock on change nothing.
        assert_eq!(
            key_route(Key::k, ctrl | ModifierType::LOCK_MASK),
            KeyRoute::Terminal
        );
    }

    /// The keys the shortcuts dialog lists do what it says.
    #[test]
    fn the_listed_terminal_keys_do_what_the_dialog_says() {
        crate::test_display::run(|| {
            for (name, accel) in super::super::terminals::TERMINAL_ACCELS {
                let (key, mods) = gtk::accelerator_parse(accel).expect(accel);
                let expected = match name {
                    "Find anything" => KeyRoute::Palette,
                    "Copy" => KeyRoute::Copy,
                    "Paste" => KeyRoute::Paste,
                    "Leave the terminal" => KeyRoute::LeaveForward,
                    other => panic!("{other} has no route"),
                };
                assert_eq!(key_route(key, mods), expected, "{name}");
            }
        });
    }

    /// The window's own shortcuts (the application's, added when the window
    /// gets its application) must come after the terminal's keys, or Ctrl+K
    /// opens Find anything instead of reaching the program. And only a
    /// focused terminal gets them.
    #[test]
    fn the_terminal_keys_run_before_the_windows_shortcuts() {
        crate::test_display::run(|| {
            let window = gtk::Window::new();
            let shortcuts = gtk::ShortcutController::new();
            shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
            shortcuts.set_name(Some("app-shortcuts"));
            window.add_controller(shortcuts);
            hand_keys_to_terminals(&window);
            let names: Vec<String> = window
                .observe_controllers()
                .iter::<gtk::glib::Object>()
                .filter_map(Result::ok)
                .filter_map(|object| object.downcast::<gtk::EventController>().ok())
                .filter(|controller| {
                    controller.propagation_phase() == gtk::PropagationPhase::Capture
                })
                .filter_map(|controller| controller.name().map(|name| name.to_string()))
                .collect();
            let at = |name: &str| names.iter().position(|seen| seen == name);
            assert!(
                at(TERMINAL_KEYS).unwrap() < at("app-shortcuts").unwrap(),
                "{names:?}"
            );

            let entry = gtk::Entry::new();
            let terminal = vte4::Terminal::new();
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            column.append(&entry);
            column.append(&terminal);
            window.set_child(Some(&column));
            window.present();
            entry.grab_focus();
            assert!(focused_terminal(&window).is_none());
            terminal.grab_focus();
            assert_eq!(focused_terminal(&window).as_ref(), Some(&terminal));
            window.destroy();
        });
    }
}
