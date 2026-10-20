//! Finding programs the way the user's own terminal would.
//!
//! DeskLink is started by D-Bus activation or the session, so its `PATH` is
//! the session's, not the one the user's shell profile builds. Agent CLIs such
//! as `claude` and `codex` install into `~/.local/bin` or an npm prefix that
//! only the profile adds, so a bare `["claude"]` from the phone was "not
//! found" while the same word typed into a shell worked.
//!
//! The login shell is asked for its `PATH` once, and bare program names are
//! resolved against it. Nothing the phone sends is ever handed to that shell:
//! the probe is a constant script, and the resolved program is executed
//! directly as an argument vector.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

/// Agent CLIs the phone may offer as one-tap sessions, by program name.
pub(crate) const KNOWN_LAUNCHERS: &[&str] = &["claude", "codex"];

/// A profile that hangs (a prompt, a network mount) must not hang DeskLink.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

const MARKER: &str = "__DESKLINK_PATH__";

/// The login shell's `PATH`, or `None` when the probe failed.
pub(crate) fn login_path() -> Option<&'static OsString> {
    static PATH: OnceLock<Option<OsString>> = OnceLock::new();
    PATH.get_or_init(probe_login_path).as_ref()
}

/// `program` as an absolute path, if it is a bare name found on the login
/// `PATH`. Anything containing a `/` is returned unchanged: the caller asked
/// for that exact file.
pub(crate) fn resolve(program: &OsStr) -> Option<PathBuf> {
    if program.as_encoded_bytes().contains(&b'/') {
        return None;
    }
    search(program, login_path()?)
}

/// Which of [`KNOWN_LAUNCHERS`] this machine can run. Cached for a minute:
/// it is asked on every session list, under the terminal locks, and a `PATH`
/// on a slow mount must not stall those.
pub(crate) fn installed() -> Vec<String> {
    static CACHE: std::sync::Mutex<Option<(Instant, Vec<String>)>> = std::sync::Mutex::new(None);
    if let Ok(cache) = CACHE.lock() {
        if let Some((at, found)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return found.clone();
            }
        }
    }
    let found: Vec<String> = if super::host::in_flatpak() {
        installed_on_host()
    } else {
        KNOWN_LAUNCHERS
            .iter()
            .filter(|name| resolve(OsStr::new(name)).is_some())
            .map(|name| name.to_string())
            .collect()
    };
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), found.clone()));
    }
    found
}

/// Inside a Flatpak the host's files are not visible, so the host's login
/// shell is asked instead. Only exact known names are believed: a profile
/// that prints a banner cannot invent a launcher.
fn installed_on_host() -> Vec<String> {
    let argv = super::host::host_probe(super::host::PROBE_INSTALLED, KNOWN_LAUNCHERS);
    let output = run_with_timeout(&argv).unwrap_or_default();
    let text = String::from_utf8_lossy(&output);
    KNOWN_LAUNCHERS
        .iter()
        .filter(|name| text.lines().any(|line| line.trim() == **name))
        .map(|name| name.to_string())
        .collect()
}

fn search(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Runs `$SHELL -lic` with a constant script that prints `PATH` between
/// markers. Interactive *and* login, because people put `PATH` edits in
/// either `.profile` or `.bashrc`/`.zshrc`; the markers skip anything a
/// profile prints.
fn probe_login_path() -> Option<OsString> {
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    let script = format!("printf '\\n{MARKER}%s{MARKER}\\n' \"$PATH\"");
    let argv: Vec<OsString> = [shell, "-l".into(), "-i".into(), "-c".into(), script.into()].to_vec();
    parse_probe(&run_with_timeout(&argv)?)
}

/// Runs `argv` with no input and returns its standard output, or `None` if
/// it could not start or outlived [`PROBE_TIMEOUT`] (it is then killed).
fn run_with_timeout(argv: &[OsString]) -> Option<Vec<u8>> {
    let (program, args) = argv.split_first()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}

fn parse_probe(output: &[u8]) -> Option<OsString> {
    let text = String::from_utf8_lossy(output);
    let start = text.find(MARKER)? + MARKER.len();
    let length = text[start..].find(MARKER)?;
    let path = &text[start..start + length];
    (!path.is_empty()).then(|| OsString::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_output_is_read_between_the_markers_only() {
        let output = b"Welcome to fortune\n__DESKLINK_PATH__/home/u/.local/bin:/usr/bin__DESKLINK_PATH__\nbye";
        assert_eq!(
            parse_probe(output),
            Some(OsString::from("/home/u/.local/bin:/usr/bin"))
        );
        assert_eq!(parse_probe(b"no markers at all"), None);
        assert_eq!(parse_probe(b"__DESKLINK_PATH____DESKLINK_PATH__"), None);
    }

    #[test]
    fn a_bare_name_is_found_on_the_given_path_and_relative_entries_are_ignored() {
        let dir = std::env::temp_dir().join(format!("desklink-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("fake-agent");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([PathBuf::from("relative"), dir.clone()]).unwrap();

        assert_eq!(
            search(OsStr::new("fake-agent"), &path),
            Some(program.clone())
        );
        assert_eq!(search(OsStr::new("missing"), &path), None);

        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(search(OsStr::new("fake-agent"), &path), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_name_with_a_slash_is_never_searched() {
        assert_eq!(resolve(OsStr::new("./claude")), None);
        assert_eq!(resolve(OsStr::new("/usr/bin/env")), None);
    }
}
