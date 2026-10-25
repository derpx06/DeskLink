//! Tests of guards against an unreadable folder or an unplaceable file need a
//! process that the file system really refuses. Root writes anywhere, so such
//! a test used to `return` there and pass without testing anything (CI runs
//! as root). [`unprivileged`] runs the test again as `nobody` instead, and
//! fails loudly when it cannot.

use std::os::unix::process::CommandExt;

const MARKER: &str = "DESKLINK_TEST_AS_NOBODY";
const NOBODY: u32 = 65534;

/// Call first in a test that needs a process the file system can refuse.
/// `true`: run the body here (not root). `false`: root re-ran this very test
/// as `nobody` and it passed; the body is not to run here. Panics when the
/// precondition cannot be met, never passes vacuously.
pub fn unprivileged(test: &str) -> bool {
    if unsafe { libc::geteuid() } != 0 {
        return true;
    }
    assert!(
        std::env::var_os(MARKER).is_none(),
        "{test}: still root after dropping privileges"
    );
    let exe = std::env::current_exe().expect("the test binary's path");
    let output = std::process::Command::new(exe)
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(MARKER, "1")
        // A scratch dir `nobody` can use, whatever TMPDIR was.
        .env("TMPDIR", "/tmp")
        .uid(NOBODY)
        .gid(NOBODY)
        .output()
        .unwrap_or_else(|error| {
            panic!("{test}: this test needs a non-root process and could not start one: {error}")
        });
    assert!(
        output.status.success(),
        "{test} failed as nobody (or the binary is not readable by it):\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[cfg(test)]
mod tests {
    /// Not root: the body runs here, no child is started.
    #[test]
    fn a_normal_user_runs_the_body_itself() {
        if unsafe { libc::geteuid() } != 0 {
            assert!(super::unprivileged("never started"));
        }
    }
}
