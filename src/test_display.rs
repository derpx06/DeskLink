//! The only way a test may build GTK widgets.
//!
//! A test must never open anything on the person's real session. Widget tests
//! run only on the dedicated headless display (an Xvfb) that
//! `DESKLINK_TEST_DISPLAY` names; they point GTK at it and away from any
//! Wayland session. Without it, or when GTK cannot start there, a widget test
//! **fails**, loudly: a green run must mean the widgets were built. The one way
//! out is the explicit `DESKLINK_TEST_NO_DISPLAY=1`, which skips them and says
//! so. GTK may be initialised from one thread only, so the answer is decided
//! once.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

/// What the environment asks of the widget tests.
#[derive(Debug, PartialEq, Eq)]
enum Wanted {
    /// Run them on this display.
    On(std::ffi::OsString),
    /// Skipped on purpose (`DESKLINK_TEST_NO_DISPLAY`).
    OptedOut,
    /// Neither a display nor an opt-out: a failure.
    Missing,
}

fn wanted(display: Option<std::ffi::OsString>, opt_out: bool) -> Wanted {
    match display.filter(|display| !display.is_empty()) {
        _ if opt_out => Wanted::OptedOut,
        Some(display) => Wanted::On(display),
        None => Wanted::Missing,
    }
}

/// The one thread GTK lives on (it may be initialised and used from a single
/// thread only, and every test runs on its own), or why there is none.
fn gtk_thread() -> &'static Result<Mutex<Sender<Job>>, String> {
    static THREAD: OnceLock<Result<Mutex<Sender<Job>>, String>> = OnceLock::new();
    THREAD.get_or_init(|| {
        let display = match wanted(
            std::env::var_os("DESKLINK_TEST_DISPLAY"),
            std::env::var_os("DESKLINK_TEST_NO_DISPLAY").is_some_and(|v| !v.is_empty()),
        ) {
            Wanted::On(display) => display,
            Wanted::OptedOut => return Err("opted out".to_string()),
            Wanted::Missing => {
                return Err("MISSING: widget tests need DESKLINK_TEST_DISPLAY to name a \
                    dedicated headless display (an Xvfb), or DESKLINK_TEST_NO_DISPLAY=1 to skip them"
                    .to_string())
            }
        };
        std::env::set_var("DISPLAY", &display);
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("GDK_BACKEND", "x11");
        let (jobs, queue) = channel::<Job>();
        let (started, ok) = channel();
        std::thread::Builder::new()
            .name("test-gtk".into())
            .spawn(move || {
                let _ = started.send(adw::init().map_err(|error| error.to_string()));
                while let Ok(job) = queue.recv() {
                    job();
                }
            })
            .map_err(|error| format!("MISSING: the GTK thread did not start: {error}"))?;
        match ok.recv() {
            Ok(Ok(())) => Ok(Mutex::new(jobs)),
            Ok(Err(error)) => Err(format!(
                "MISSING: GTK could not start on DESKLINK_TEST_DISPLAY={display:?} \
                 (is the Xvfb running?): {error}"
            )),
            Err(_) => Err("MISSING: the GTK thread died while starting".to_string()),
        }
    })
}

/// Runs `body` on the GTK thread of a dedicated headless display and waits
/// for it (a panic in it fails the calling test). Without the display it
/// fails the test, unless `DESKLINK_TEST_NO_DISPLAY` skips widget tests on
/// purpose.
pub(crate) fn run(body: impl FnOnce() + Send + 'static) {
    let thread = match gtk_thread() {
        Ok(thread) => thread,
        Err(why) if why.starts_with("MISSING") => panic!("{why}"),
        Err(_) => {
            eprintln!("[test] widget test skipped: DESKLINK_TEST_NO_DISPLAY is set");
            return;
        }
    };
    let (done, finished) = channel();
    let job: Job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let _ = done.send(result);
    });
    thread
        .lock()
        .map(|jobs| jobs.send(job))
        .expect("the GTK test thread's lock")
        .expect("the GTK test thread is gone");
    match finished.recv() {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("the GTK test thread died before the test finished"),
    }
}

#[cfg(test)]
mod tests {
    use super::{wanted, Wanted};
    use std::ffi::OsString;

    /// A missing display is a failure, an empty one is missing, and only the
    /// explicit opt-out skips.
    #[test]
    fn a_missing_display_is_a_failure_unless_opted_out() {
        assert_eq!(wanted(None, false), Wanted::Missing);
        assert_eq!(wanted(Some(OsString::new()), false), Wanted::Missing);
        assert_eq!(wanted(None, true), Wanted::OptedOut);
        assert_eq!(wanted(Some(":61".into()), true), Wanted::OptedOut);
        assert_eq!(wanted(Some(":61".into()), false), Wanted::On(":61".into()));
    }
}
