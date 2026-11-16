//! Dark mode on desktops that switch it with the GTK theme. Xfce, MATE and
//! Cinnamon go dark by choosing a dark GTK theme ("Adwaita-dark",
//! "Mint-Y-Dark"): their portal reports no colour-scheme preference, and
//! libadwaita replaces the GTK theme name with its own, so DeskLink stayed
//! light under a dark desktop. The desktop's own theme setting is read here
//! and followed while Appearance is "System". Elsewhere (GNOME, Plasma,
//! anything with a colour-scheme portal) nothing is watched.

use std::cell::Cell;

use gtk::gio;
use gtk::prelude::*;

thread_local! {
    /// The desktop's GTK theme is a dark one (as last read).
    static DARK: Cell<bool> = const { Cell::new(false) };
}

/// Whether a GTK theme name is a dark variant ("Adwaita-dark",
/// "Mint-Y-Dark-Aqua", "Arc-Dark", "Yaru-dark").
pub(crate) fn is_dark_theme(name: &str) -> bool {
    name.to_ascii_lowercase().contains("dark")
}

/// What "System" means: the desktop's colour scheme, and dark when the
/// desktop's GTK theme is dark (a desktop asking for light still wins).
pub(crate) fn system_scheme() -> adw::ColorScheme {
    if DARK.with(Cell::get) {
        adw::ColorScheme::PreferDark
    } else {
        adw::ColorScheme::Default
    }
}

/// Reads the desktop's GTK theme now and on every change, calling `changed`
/// after each change.
pub(crate) fn follow(changed: impl Fn() + 'static) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let set = move |name: &str| {
        DARK.with(|dark| dark.set(is_dark_theme(name)));
        changed();
    };
    if desktop.split(':').any(|name| name == "xfce") {
        follow_xfconf(set);
    } else if desktop.split(':').any(|name| name == "mate") {
        follow_gsettings("org.mate.interface", set);
    } else if desktop.split(':').any(|name| name.ends_with("cinnamon")) {
        follow_gsettings("org.cinnamon.desktop.interface", set);
    }
}

/// MATE and Cinnamon keep the theme in their own GSettings schema.
fn follow_gsettings(schema: &str, set: impl Fn(&str) + 'static) {
    let installed = gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup(schema, true))
        .is_some_and(|found| found.has_key("gtk-theme"));
    if !installed {
        return;
    }
    let settings = gio::Settings::new(schema);
    set(&settings.string("gtk-theme"));
    settings.connect_changed(Some("gtk-theme"), move |settings, _| {
        set(&settings.string("gtk-theme"))
    });
    // Kept for the life of the process: dropping it ends the watch.
    std::mem::forget(settings);
}

/// Xfce keeps it in xfconf's "xsettings" channel, read over D-Bus.
fn follow_xfconf(set: impl Fn(&str) + 'static) {
    const NAME: &str = "org.xfce.Xfconf";
    const PATH: &str = "/org/xfce/Xfconf";
    const PROPERTY: &str = "/Net/ThemeName";
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    let set = std::rc::Rc::new(set);
    {
        let set = set.clone();
        bus.call(
            Some(NAME),
            PATH,
            NAME,
            "GetProperty",
            Some(&("xsettings", PROPERTY).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            gio::Cancellable::NONE,
            move |reply| {
                if let Some(name) = reply
                    .ok()
                    .and_then(|reply| reply.child_value(0).as_variant())
                    .and_then(|value| value.get::<String>())
                {
                    set(&name);
                }
            },
        );
    }
    bus.signal_subscribe(
        Some(NAME),
        Some(NAME),
        Some("PropertyChanged"),
        Some(PATH),
        Some("xsettings"),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, args| {
            let (channel, property) = (args.child_value(0), args.child_value(1));
            if channel.str() == Some("xsettings") && property.str() == Some(PROPERTY) {
                if let Some(name) = args
                    .child_value(2)
                    .as_variant()
                    .and_then(|value| value.get::<String>())
                {
                    set(&name);
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn dark_variants_are_recognised_by_name() {
        for dark in [
            "Adwaita-dark",
            "Mint-Y-Dark-Aqua",
            "Arc-Dark",
            "Yaru-dark",
            "Breeze-Dark",
        ] {
            assert!(super::is_dark_theme(dark), "{dark}");
        }
        for light in ["Adwaita", "Mint-Y", "Greybird", "Yaru", "TraditionalOk", ""] {
            assert!(!super::is_dark_theme(light), "{light}");
        }
    }

    #[test]
    fn system_follows_the_last_theme_read() {
        super::DARK.with(|dark| dark.set(true));
        assert_eq!(super::system_scheme(), adw::ColorScheme::PreferDark);
        super::DARK.with(|dark| dark.set(false));
        assert_eq!(super::system_scheme(), adw::ColorScheme::Default);
    }
}
