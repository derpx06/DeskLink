/* main.rs
 *
 * Copyright 2026 manas
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

/// Diagnostics: always in debug builds, and in release builds only with
/// `DESKLINK_DEBUG=1`. Errors a user should see stay `eprintln!`.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug_logging() {
            eprintln!($($arg)*);
        }
    };
}

pub(crate) fn debug_logging() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(debug_assertions) || std::env::var("DESKLINK_DEBUG").as_deref() == Ok("1")
    })
}

mod application;
mod autostart;
mod config;
mod desk_menu;
mod device_links;
mod kde;
mod link_glyph;
mod phone_mpris;
mod protocol;
mod remote_control;
mod search_provider;
mod send_to;
mod shell_dbus;
mod terminal_previews;
mod thumbnails;
mod tray;
mod window;

use self::application::DesklinkApplication;
use self::window::DesklinkWindow;

use config::{GETTEXT_PACKAGE, LOCALEDIR};
use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};
use gtk::prelude::*;
use gtk::{gio, glib};

fn main() -> glib::ExitCode {
    // `desklink --attach-terminal <id>`: the client a desk terminal window
    // runs to show a shared session. No GTK, no second app instance.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--attach-terminal" {
        let status = device_links::pty::desk::run_client(&args[2]);
        return glib::ExitCode::from(status.clamp(0, 255));
    }
    // `desklink --send-files [--to <phone>] <files…>`: Files' "Send to"
    // scripts and Open With entry. Hands the files to the running app.
    if args.get(1).is_some_and(|arg| arg == "--send-files") {
        return send_to::run_cli(&args[2..]);
    }
    // `desklink --dial <number>`, or a `tel:` link from a launcher that does
    // not use D-Bus activation: the running DeskLink asks, then the phone
    // calls. Handled here, so a second instance never starts.
    if args.len() == 3 && args[1] == "--dial" {
        return send_to::run_dial_cli(&args[2]);
    }
    if args.len() == 2
        && args[1]
            .get(..4)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("tel:"))
    {
        return send_to::run_dial_cli(&args[1]);
    }

    install_crash_reporter();

    // Set up gettext translations
    bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8")
        .expect("Unable to set the text domain encoding");
    textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    // Load resources
    let resources = load_resources().expect("Could not load resources");
    gio::resources_register(&resources);

    // Create a new GtkApplication. The application manages our main loop,
    // application windows, integration with the window manager/compositor, and
    // desktop features such as file opening and single-instance applications.
    // HANDLES_OPEN: `tel:` links arrive as `open` (see `application.rs`).
    let app = DesklinkApplication::new("derx06.desklink.com", &gio::ApplicationFlags::HANDLES_OPEN);

    // Run the application. This function will block until the application
    // exits. Upon return, we have our exit code to return to the shell. (This
    // is the code you see when you do `echo $?` after running a command in a
    // terminal.
    app.run()
}

fn load_resources() -> Result<gio::Resource, glib::Error> {
    let bytes = glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/desklink.gresource"
    )));
    gio::Resource::from_data(&bytes)
}

/// Where crash reports go: `$XDG_STATE_HOME/desklink/crash-<unix time>.txt`.
pub(crate) fn crash_dir() -> Option<std::path::PathBuf> {
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink"),
    )
}

/// Writes every panic, with a backtrace, to a file the user can attach to a
/// bug report (nothing is sent anywhere), then runs the default hook. The next
/// start mentions it (`application.rs`).
fn install_crash_reporter() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = crash_dir() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();
            let report = format!(
                "DeskLink {} crashed\nthread: {}\n{info}\n\n{}\n",
                config::VERSION,
                std::thread::current().name().unwrap_or("unnamed"),
                std::backtrace::Backtrace::force_capture()
            );
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(format!("crash-{now}.txt")), report);
        }
        default(info);
    }));
}
