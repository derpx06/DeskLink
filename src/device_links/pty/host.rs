//! Shared terminals inside a Flatpak sandbox.
//!
//! A terminal the phone opens must be the user's real shell on the real
//! machine, not one inside the sandbox. In a Flatpak every program is
//! therefore started on the host with `flatpak-spawn --host`, which hands it
//! the PTY as its standard streams. Two details make it a proper terminal:
//!
//! * `setsid --ctty --wait` makes the PTY the host shell's *controlling*
//!   terminal, without which Ctrl+C and job control do nothing; `--wait` keeps
//!   `flatpak-spawn` alive until the shell exits (setsid forks when it is
//!   already a group leader).
//! * `--watch-bus` ends the host process if DeskLink goes away, so no shell is
//!   left orphaned on the host.
//!
//! Program names from the phone are never put into a shell string: the host
//! scripts below are constants and the phone's argv follows as positional
//! arguments.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Whether DeskLink runs inside a Flatpak sandbox.
pub(crate) fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Runs the user's login shell (`$SHELL`, falling back to `/bin/sh`).
const LOGIN_SHELL: &str = r#"exec "${SHELL:-/bin/sh}" -l"#;

/// Runs `"$@"` through the user's interactive login shell, so a bare name
/// (`claude`) resolves on the PATH the user's profile builds.
const LOGIN_EXEC: &str = r#"exec "${SHELL:-/bin/sh}" -lic 'exec "$0" "$@"' "$@""#;

/// Runs the script in `$1` through the user's login shell with the rest of
/// the arguments.
const PROBE_RUNNER: &str = r#"s=$1; shift; exec "${SHELL:-/bin/sh}" -lic "$s" sh "$@""#;

/// Lists which of the given names the user's login shell can run.
pub(crate) const PROBE_INSTALLED: &str =
    r#"for p in "$@"; do command -v "$p" >/dev/null 2>&1 && printf '%s\n' "$p"; done"#;

fn host_prefix() -> Vec<OsString> {
    [
        "flatpak-spawn",
        "--host",
        "--watch-bus",
        "--env=TERM=xterm-256color",
        "--env=COLORTERM=truecolor",
        "setsid",
        "--ctty",
        "--wait",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// argv that runs the user's login shell on the host, in this PTY.
pub(crate) fn host_login_shell() -> Vec<OsString> {
    let mut argv = host_prefix();
    argv.extend(["sh", "-c", LOGIN_SHELL].map(OsString::from));
    argv
}

/// argv that runs `command` on the host, in this PTY, through the login shell.
pub(crate) fn host_command(command: &[OsString]) -> Vec<OsString> {
    let mut argv = host_prefix();
    argv.extend(["sh", "-c", LOGIN_EXEC, "sh"].map(OsString::from));
    argv.extend(command.iter().cloned());
    argv
}

/// argv for a one-off host command whose output DeskLink reads (no PTY).
pub(crate) fn host_probe(script: &str, args: &[&str]) -> Vec<OsString> {
    // The script travels as an argument too, so its own quotes need no
    // escaping: the outer shell hands it to the login shell's `-c`.
    let mut argv: Vec<OsString> = ["flatpak-spawn", "--host", "sh", "-c", PROBE_RUNNER, "sh"]
        .map(OsString::from)
        .to_vec();
    argv.push(script.into());
    argv.extend(args.iter().map(OsString::from));
    argv
}

/// Where desk terminal windows find this DeskLink. Inside a Flatpak only the
/// app's own runtime folder is shared between its instances (the window's
/// `flatpak run --command=desklink` is a second instance).
pub(crate) fn runtime_dir() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    Some(
        match std::env::var_os("FLATPAK_ID").filter(|_| in_flatpak()) {
            Some(app_id) => base.join("app").join(app_id),
            None => base,
        },
    )
}

/// The command a host terminal window runs to show session `id`.
pub(crate) fn attach_client(exe: &str, id: &str) -> Vec<String> {
    match std::env::var("FLATPAK_ID").ok().filter(|_| in_flatpak()) {
        Some(app_id) => [
            "flatpak",
            "run",
            "--command=desklink",
            app_id.as_str(),
            "--attach-terminal",
            id,
        ]
        .map(str::to_string)
        .to_vec(),
        None => [exe, "--attach-terminal", id].map(str::to_string).to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(argv: &[OsString]) -> Vec<String> {
        argv.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn phone_argv_stays_positional_after_a_constant_script() {
        let argv = strings(&host_command(&["claude".into(), "--continue".into()]));
        assert_eq!(argv[0], "flatpak-spawn");
        assert!(argv.contains(&"--watch-bus".to_string()));
        let ctty = argv.iter().position(|arg| arg == "--ctty").unwrap();
        assert_eq!(argv[ctty - 1], "setsid");
        let script = argv.iter().position(|arg| arg == "-c").unwrap() + 1;
        assert_eq!(argv[script], LOGIN_EXEC);
        assert_eq!(&argv[script + 1..], ["sh", "claude", "--continue"]);
    }

    #[test]
    fn a_hostile_name_is_an_argument_not_shell_text() {
        let argv = strings(&host_command(&["$(rm -rf ~)".into()]));
        assert_eq!(argv.last().unwrap(), "$(rm -rf ~)");
        assert!(!argv
            .iter()
            .any(|arg| arg.contains("rm -rf") && arg.contains("exec")));
    }

    #[test]
    fn the_probe_passes_names_as_arguments() {
        let argv = strings(&host_probe(PROBE_INSTALLED, &["claude", "codex"]));
        assert_eq!(&argv[..2], ["flatpak-spawn", "--host"]);
        assert_eq!(
            &argv[argv.len() - 3..],
            [PROBE_INSTALLED, "claude", "codex"]
        );
    }
}
