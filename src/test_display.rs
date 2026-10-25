//! The only way a test may build GTK widgets.
//!
//! A test must never open anything on the person's real session. Widget tests
//! run only when `DESKLINK_TEST_DISPLAY` names a dedicated headless display
//! (an Xvfb); they then point GTK at it and away from any Wayland session.
//! Otherwise they skip, saying so. GTK may be initialised from one thread
//! only, so the answer is decided once.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

type Job = Box<dyn FnOnce() + Send>;

/// The one thread GTK lives on (it may be initialised and used from a single
/// thread only, and every test runs on its own): `None` without a dedicated
/// display.
fn gtk_thread() -> Option<&'static Mutex<Sender<Job>>> {
    static THREAD: OnceLock<Option<Mutex<Sender<Job>>>> = OnceLock::new();
    THREAD
        .get_or_init(|| {
            let Some(display) = std::env::var_os("DESKLINK_TEST_DISPLAY").filter(|d| !d.is_empty())
            else {
                eprintln!(
                    "[test] widget test skipped: set DESKLINK_TEST_DISPLAY to a headless display (Xvfb)"
                );
                return None;
            };
            std::env::set_var("DISPLAY", display);
            std::env::remove_var("WAYLAND_DISPLAY");
            std::env::set_var("GDK_BACKEND", "x11");
            let (jobs, queue) = channel::<Job>();
            let (started, ok) = channel();
            std::thread::Builder::new()
                .name("test-gtk".into())
                .spawn(move || {
                    let _ = started.send(adw::init().is_ok());
                    while let Ok(job) = queue.recv() {
                        job();
                    }
                })
                .ok()?;
            ok.recv().ok().filter(|ok| *ok)?;
            Some(Mutex::new(jobs))
        })
        .as_ref()
}

/// Runs `body` on the GTK thread of a dedicated headless display and waits
/// for it (a panic in it fails the calling test). Without the display it
/// says so and does nothing.
pub(crate) fn run(body: impl FnOnce() + Send + 'static) {
    let Some(thread) = gtk_thread() else {
        return;
    };
    let (done, finished) = channel();
    let job: Job = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let _ = done.send(result);
    });
    if thread.lock().map(|jobs| jobs.send(job)).is_err() {
        return;
    }
    if let Ok(Err(panic)) = finished.recv() {
        std::panic::resume_unwind(panic);
    }
}

/// Whether widget tests can run here.
#[cfg(test)]
pub(crate) fn available() -> bool {
    gtk_thread().is_some()
}

#[cfg(test)]
mod tests {
    /// Without `DESKLINK_TEST_DISPLAY` nothing is touched: the helper says no.
    #[test]
    fn widget_tests_skip_without_a_dedicated_display() {
        if std::env::var_os("DESKLINK_TEST_DISPLAY").is_none() {
            assert!(!super::available());
        }
    }
}
