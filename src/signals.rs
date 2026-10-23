//! SIGTERM, SIGINT and SIGHUP end DeskLink the way Quit does, with a way out
//! when that cannot happen.
//!
//! The handler only writes the signal's number to a pipe (all that is safe in
//! a handler). A thread started at startup reads it and asks the application
//! to quit on the main loop; if the main loop is wedged, or quitting hangs, the
//! process exits anyway after a grace period, and a second signal ends it at
//! once. GLib's own signal sources are not used: they run on the main loop, so
//! a wedged main thread could never be ended by `kill`.

use std::os::raw::c_int;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// How long quitting may take before the process is ended regardless.
pub(crate) const GRACE: Duration = Duration::from_secs(5);

/// The pipe's write end, for the handler.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: write(2) is async-signal-safe; the byte outlives the call.
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

/// What a signal makes happen, apart from the process being left to quit:
/// told to the caller so it can be tested.
pub(crate) trait Actions: Send + 'static {
    /// Ask for a clean quit (on the main loop).
    fn quit(&self, signal: c_int);
    /// End the process now.
    fn exit(&self, signal: c_int);
}

/// The real thing: the application quits; `_exit` ends the process.
pub(crate) struct Real;

impl Actions for Real {
    fn quit(&self, _signal: c_int) {
        gtk::glib::MainContext::default().invoke(|| {
            use gtk::prelude::ApplicationExt;
            if let Some(app) = gtk::gio::Application::default() {
                app.quit();
            }
        });
    }

    fn exit(&self, signal: c_int) {
        // SAFETY: _exit ends the process without running anything else.
        unsafe { libc::_exit(128 + signal) }
    }
}

/// Handles `signals`: the first asks `actions` to quit and starts the grace
/// period; the end of it, or a second signal, exits.
pub(crate) fn watch(
    signals: &[c_int],
    grace: Duration,
    actions: impl Actions,
) -> std::io::Result<()> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let [read_fd, write_fd] = fds;
    if WRITE_FD
        .compare_exchange(-1, write_fd, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // SAFETY: both descriptors were just made here.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err(std::io::Error::other("signals are already watched"));
    }
    std::thread::Builder::new()
        .name("desklink-signals".into())
        .spawn(move || {
            let mut asked: Option<c_int> = None;
            loop {
                let mut byte = 0u8;
                // Waits for a signal, or (once one came) for the grace to end.
                let ready = match asked {
                    None => true,
                    Some(_) => {
                        let mut poll = libc::pollfd {
                            fd: read_fd,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: one valid pollfd.
                        unsafe { libc::poll(&mut poll, 1, grace.as_millis() as c_int) > 0 }
                    }
                };
                if !ready {
                    actions.exit(asked.unwrap_or(0));
                    return;
                }
                // SAFETY: reads one byte into a local.
                let got = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
                if got != 1 {
                    return;
                }
                let signal = c_int::from(byte);
                if asked.is_some() {
                    // A second one: no more waiting.
                    actions.exit(signal);
                    return;
                }
                asked = Some(signal);
                actions.quit(signal);
            }
        })?;
    for signal in signals {
        // SAFETY: a plain handler that only calls write(2).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(*signal, &action, std::ptr::null_mut());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Sender};

    struct Record(Sender<(&'static str, c_int)>);

    impl Actions for Record {
        fn quit(&self, signal: c_int) {
            let _ = self.0.send(("quit", signal));
        }
        fn exit(&self, signal: c_int) {
            let _ = self.0.send(("exit", signal));
        }
    }

    /// A signal asks for a quit; with the main loop not answering, the
    /// process is ended after the grace, and a second signal ends it at once.
    /// (SIGUSR1 stands in: nothing else in the tests uses it.)
    #[test]
    fn a_signal_quits_and_a_wedged_quit_is_ended_after_the_grace() {
        let (events, seen) = channel();
        watch(&[libc::SIGUSR1], Duration::from_millis(300), Record(events)).unwrap();
        // SAFETY: raise is safe; the handler is installed.
        unsafe { libc::raise(libc::SIGUSR1) };
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(2)),
            Ok(("quit", libc::SIGUSR1))
        );
        // Nothing quits the "process": the grace runs out.
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(2)),
            Ok(("exit", libc::SIGUSR1))
        );
        // Watching twice is refused (one handler for the process).
        let (other, _) = channel();
        assert!(watch(&[libc::SIGUSR1], GRACE, Record(other)).is_err());
    }
}
