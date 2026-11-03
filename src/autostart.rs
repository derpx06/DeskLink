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

/// The autostart entry for a native install. `executable` is an absolute
/// path: a bare name would be searched on the login manager's `PATH`, which
/// often lacks `~/.local/bin`, and systemd's autostart generator drops an
/// entry whose `TryExec` it cannot find.
fn entry(executable: &str) -> String {
    // The `Exec` path is quoted (an AppImage's mount point or a home folder
    // can hold spaces). `TryExec` makes every desktop skip the entry once the
    // program is gone, so an uninstall leaves no broken autostart behind; it
    // is written only for a path that cannot be misread as a name to search.
    let exec = if executable.contains(char::is_whitespace) {
        crate::send_to::desktop_exec_quoted(std::path::Path::new(executable))
    } else {
        executable.to_string()
    };
    let try_exec = if executable.starts_with('/') {
        format!("TryExec={executable}\n")
    } else {
        String::new()
    };
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=DeskLink\n\
         Comment=Keep this computer reachable from your phone\n\
         {try_exec}\
         Exec={exec} --gapplication-service\n\
         Icon={APP_ID}\n\
         X-GNOME-Autostart-enabled=true\n\
         NoDisplay=true\n"
    )
}

/// The absolute path to start at login: the installed `desklink` on `PATH`
/// when there is one (it survives an upgrade that moves this binary), else
/// the running binary.
fn executable(path_var: Option<std::ffi::OsString>, current: Option<PathBuf>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path_var
        .iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("desklink"))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .or(current)
        .filter(|path| path.is_absolute())
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
    let executable = executable(std::env::var_os("PATH"), std::env::current_exe().ok())
        .ok_or("DeskLink could not find its own program to start at login")?
        .to_string_lossy()
        .into_owned();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, entry(&executable)).map_err(|error| error.to_string())
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
    use std::path::PathBuf;

    #[test]
    fn the_entry_starts_the_background_service_without_a_window() {
        let entry = super::entry("/usr/bin/desklink");
        assert!(entry.contains("Exec=/usr/bin/desklink --gapplication-service\n"));
        assert!(entry.contains("TryExec=/usr/bin/desklink\n"));
        assert!(entry.contains("NoDisplay=true"));
    }

    #[test]
    fn the_entry_skips_itself_once_uninstalled_and_quotes_odd_paths() {
        let spaced = super::entry("/tmp/My Apps/desklink");
        assert!(spaced.contains("TryExec=/tmp/My Apps/desklink\n"));
        assert!(spaced.contains("Exec=\"/tmp/My Apps/desklink\" --gapplication-service\n"));
    }

    #[test]
    fn a_name_that_is_not_a_path_gets_no_tryexec() {
        // A bare name would be looked up on the login manager's PATH.
        let bare = super::entry("desklink");
        assert!(!bare.contains("TryExec"));
        assert!(bare.contains("Exec=desklink --gapplication-service\n"));
    }

    #[test]
    fn the_program_is_an_absolute_path_found_on_path_or_the_running_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("desklink-autostart-{}", std::process::id()));
        let bin = dir.join(".local").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("desklink");
        std::fs::write(&installed, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            std::env::join_paths([std::path::Path::new("relative/bin"), bin.as_path()]).unwrap();
        let running = PathBuf::from("/opt/desklink/desklink");
        // On PATH: that one, absolute (a relative PATH entry is ignored).
        assert_eq!(
            super::executable(Some(path), Some(running.clone())),
            Some(installed)
        );
        // Not on PATH: the running binary.
        assert_eq!(
            super::executable(Some("/nonexistent".into()), Some(running.clone())),
            Some(running)
        );
        // Neither, or only a relative path: nothing, never a bare name.
        assert_eq!(super::executable(None, None), None);
        assert_eq!(
            super::executable(None, Some(PathBuf::from("desklink"))),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
