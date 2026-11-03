//! Starting DeskLink at login, so a phone can reach this computer after a
//! reboot without anyone opening the app first.
//!
//! A native install writes an XDG autostart entry that starts the background
//! service (no window). A Flatpak cannot write there; it asks the Background
//! portal, which records the autostart entry on the app's behalf.

use std::path::PathBuf;

use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ToVariant;

const APP_ID: &str = "derx06.desklink.com";

fn in_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

fn entry_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("autostart")
            .join(format!("{APP_ID}.desktop")),
    )
}

/// The autostart entry for a native install.
fn entry(executable: &str) -> String {
    // The `Exec` path is quoted (an AppImage's mount point or a home folder
    // can hold spaces). `TryExec` makes every desktop skip the entry once the
    // program is gone, so an uninstall leaves no broken autostart behind.
    let exec = if executable.contains(char::is_whitespace) {
        crate::send_to::desktop_exec_quoted(std::path::Path::new(executable))
    } else {
        executable.to_string()
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=DeskLink\n\
         Comment=Keep this computer reachable from your phone\n\
         TryExec={executable}\n\
         Exec={exec} --gapplication-service\n\
         Icon={APP_ID}\n\
         X-GNOME-Autostart-enabled=true\n\
         NoDisplay=true\n"
    )
}

/// Turns starting at login on or off.
pub fn set(enabled: bool) -> Result<(), String> {
    if in_flatpak() {
        return request_background(enabled);
    }
    let path = entry_path().ok_or("No configuration directory")?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        };
    }
    // The installed command when there is one on PATH; otherwise this binary.
    let executable = if which("desklink") {
        "desklink".to_string()
    } else {
        std::env::current_exe()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .into_owned()
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, entry(&executable)).map_err(|error| error.to_string())
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// `org.freedesktop.portal.Background.RequestBackground`. Fire and forget:
/// the portal may show its own dialog, and its answer arrives later as a
/// signal nobody needs here.
fn request_background(enabled: bool) -> Result<(), String> {
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|error| error.to_string())?;
    let options = glib::VariantDict::new(None);
    options.insert_value(
        "reason",
        &"Keep this computer reachable from your phone".to_variant(),
    );
    options.insert_value("autostart", &enabled.to_variant());
    options.insert_value(
        "commandline",
        &vec!["desklink", "--gapplication-service"].to_variant(),
    );
    options.insert_value("dbus-activatable", &false.to_variant());
    bus.call(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Background",
        "RequestBackground",
        Some(&("", options.end()).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        None::<&gio::Cancellable>,
        |reply| {
            if let Err(error) = reply {
                // No Background portal (no xdg-desktop-portal in this session).
                eprintln!("[DeskLink] Start at login could not be set: {error}");
            }
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_entry_starts_the_background_service_without_a_window() {
        let entry = super::entry("/usr/bin/desklink");
        assert!(entry.contains("Exec=/usr/bin/desklink --gapplication-service\n"));
        assert!(entry.contains("NoDisplay=true"));
    }

    #[test]
    fn the_entry_skips_itself_once_uninstalled_and_quotes_odd_paths() {
        let plain = super::entry("desklink");
        assert!(plain.contains("TryExec=desklink\n"));
        let spaced = super::entry("/tmp/My Apps/desklink");
        assert!(spaced.contains("TryExec=/tmp/My Apps/desklink\n"));
        assert!(spaced.contains("Exec=\"/tmp/My Apps/desklink\" --gapplication-service\n"));
    }
}
