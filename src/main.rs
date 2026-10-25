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

/// Startup timing, for `DESKLINK_DEBUG=1`: each stage with the milliseconds
/// since the process started (read from the kernel, so it includes the
/// dynamic linking and everything before `main`).
pub(crate) fn startup(stage: &str) {
    if !debug_logging() {
        return;
    }
    // The process start is when the shell that ran it was forked, so the
    // figure to read is the one since `main`.
    static MAIN: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    let now = since_process_start_ms();
    let main = *MAIN.get_or_init(|| now);
    eprintln!(
        "[Startup] {:>5.0} ms since main ({:>6.0} ms since the process was forked)  {stage}",
        now - main,
        now
    );
}

fn since_process_start_ms() -> f64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let started = std::fs::read_to_string("/proc/self/stat")
        .ok()
        // The command name may hold spaces and brackets: count from its end.
        .and_then(|stat| {
            let after = stat.rsplit_once(')')?.1.to_string();
            after.split_whitespace().nth(19)?.parse::<f64>().ok()
        });
    let up = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|up| up.split_whitespace().next()?.parse::<f64>().ok());
    match (started, up) {
        (Some(started), Some(up)) if ticks > 0.0 => ((up - started / ticks) * 1000.0).max(0.0),
        _ => 0.0,
    }
}

mod application;
mod autostart;
mod bucket_progress;
mod config;
mod desk_menu;
mod device_links;
mod image_limits;
mod kde;
mod link_glyph;
mod pdf;
mod phone_mpris;
mod protocol;
mod remote_control;
mod search_provider;
mod send_to;
#[cfg(test)]
mod shared_fixtures;
mod shell_dbus;
mod signals;
mod terminal_previews;
#[cfg(test)]
mod test_display;
#[cfg(test)]
mod test_privileges;
mod thumbnails;
mod ticker;
mod tray;
mod type_on_phone;
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
    // `desklink --bucket [--to <phone>] <files…>`: into the Bucket (F8b).
    if args.get(1).is_some_and(|arg| arg == "--bucket") {
        return send_to::run_bucket_cli(&args[2..]);
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

    startup("main");
    install_crash_reporter();

    // Set up gettext translations
    bindtextdomain(GETTEXT_PACKAGE, LOCALEDIR).expect("Unable to bind the text domain");
    bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8")
        .expect("Unable to set the text domain encoding");
    textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    // Load resources
    let resources = load_resources().expect("Could not load resources");
    gio::resources_register(&resources);
    startup("resources loaded");

    // Create a new GtkApplication. The application manages our main loop,
    // application windows, integration with the window manager/compositor, and
    // desktop features such as file opening and single-instance applications.
    // HANDLES_OPEN: `tel:` links arrive as `open` (see `application.rs`).
    let app = DesklinkApplication::new("derx06.desklink.com", &gio::ApplicationFlags::HANDLES_OPEN);

    // Run the application. This function will block until the application
    // exits. Upon return, we have our exit code to return to the shell. (This
    // is the code you see when you do `echo $?` after running a command in a
    // terminal.
    startup("application created; running");
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

/// Crash reports kept: the newest this many.
const CRASH_REPORTS_KEPT: usize = 20;

thread_local! {
    /// Depth of `guard()` calls on this thread: a panic inside one is caught
    /// and the app carries on, so it is not a crash.
    static GUARDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether this thread is inside a `guard()`.
fn guarded() -> bool {
    GUARDED.with(|depth| depth.get() > 0)
}

/// A report file name nobody else gets: the time to the nanosecond, the
/// process and a counter, so two panics in one second never overwrite.
fn crash_file_name(secs: u64, nanos: u32, pid: u32, counter: u64) -> String {
    format!("crash-{secs}-{nanos:09}-{pid}-{counter}.txt")
}

/// Whether `name` is a crash report (not `crash-reported`, the marker).
pub(crate) fn is_crash_report(name: &str) -> bool {
    name.starts_with("crash-") && name.ends_with(".txt")
}

/// Keeps the newest `keep` crash reports in `dir`.
fn prune_crash_reports(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|entry| is_crash_report(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    reports.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in reports.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Writes a panic that really ends the app, with a backtrace, to a file the
/// user can attach to a bug report (nothing is sent anywhere), then runs the
/// default hook. Only the first one of a process is kept (what follows is
/// usually its echo), the directory keeps the newest [`CRASH_REPORTS_KEPT`],
/// and a panic that a `guard()` catches is a log line, never a report. The
/// next start mentions it (`application.rs`).
fn install_crash_reporter() {
    static WROTE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if guarded() {
            device_links::limits::log_limited(
                "caught-panic",
                &format!("A handler failed and was caught: {info}"),
            );
            return;
        }
        if let Some(dir) = crash_dir() {
            if !WROTE.swap(true, std::sync::atomic::Ordering::AcqRel) {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let report = format!(
                    "DeskLink {} crashed\nthread: {}\n{info}\n\n{}\n",
                    config::VERSION,
                    std::thread::current().name().unwrap_or("unnamed"),
                    std::backtrace::Backtrace::force_capture()
                );
                let _ = device_links::private_fs::create_dir_all(&dir);
                prune_crash_reports(&dir, CRASH_REPORTS_KEPT - 1);
                let name = crash_file_name(
                    now.as_secs(),
                    now.subsec_nanos(),
                    std::process::id(),
                    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                );
                let _ = device_links::private_fs::write(&dir.join(name), report.as_bytes());
            }
        }
        default(info);
    }));
}

/// Runs `body`, turning a panic into a logged error and `fallback`. For GLib
/// callbacks fed by other processes or peers (D-Bus calls and signals, the
/// timers that read what peers sent): a panic cannot unwind into C, so it
/// would abort the whole app. The crash hook records it in the log only.
pub(crate) fn guard<T>(name: &str, fallback: T, body: impl FnOnce() -> T) -> T {
    GUARDED.with(|depth| depth.set(depth.get() + 1));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    GUARDED.with(|depth| depth.set(depth.get().saturating_sub(1)));
    result.unwrap_or_else(|_| {
        eprintln!("[DeskLink] The {name} handler failed; carrying on");
        fallback
    })
}

#[cfg(test)]
mod guard_tests {
    #[test]
    fn a_panicking_callback_returns_its_fallback() {
        assert_eq!(super::guard("test", 7, || -> i32 { panic!("boom") }), 7);
        assert_eq!(super::guard("test", 7, || 3), 3);
    }

    #[test]
    fn crash_reports_never_share_a_name_and_the_marker_is_not_one() {
        let a = super::crash_file_name(1_700_000_000, 5, 42, 0);
        let b = super::crash_file_name(1_700_000_000, 5, 42, 1);
        let c = super::crash_file_name(1_700_000_000, 6, 42, 0);
        assert!(a != b && a != c);
        assert!(super::is_crash_report(&a));
        assert!(!super::is_crash_report("crash-reported"));
        assert!(!super::is_crash_report("crash-1700000000.log"));
    }

    #[test]
    fn only_the_newest_twenty_reports_are_kept() {
        let dir = std::env::temp_dir().join(format!("desklink-crash-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..30u64 {
            let path = dir.join(super::crash_file_name(1_700_000_000 + n, 0, 1, 0));
            std::fs::write(&path, b"x").unwrap();
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + n);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        std::fs::write(dir.join("crash-reported"), b"").unwrap();
        super::prune_crash_reports(&dir, super::CRASH_REPORTS_KEPT);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| super::is_crash_report(n))
            .collect();
        left.sort();
        assert_eq!(left.len(), 20);
        assert!(left[0].contains("-1700000010-") || left[0].starts_with("crash-1700000010"));
        assert!(dir.join("crash-reported").exists(), "the marker stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A panic inside `guard` is caught and is not a crash; the depth is
    /// restored whether the body panics or not.
    #[test]
    fn a_guarded_panic_is_not_a_crash() {
        assert!(!super::guarded());
        assert_eq!(super::guard("t", 1, || -> i32 { panic!("caught") }), 1);
        assert!(!super::guarded());
        assert!(super::guard("t", false, super::guarded));
        assert!(!super::guarded());
    }
}
