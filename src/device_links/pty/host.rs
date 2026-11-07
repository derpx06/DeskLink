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

/// This program, to start again as a desk terminal's client. After an update
/// replaced the binary while DeskLink ran (`cp` then `mv`), Linux names the
/// running one ".../desklink (deleted)", and starting that fails: the new file
/// at the same path is the one to run. `this_process`: the child is this
/// process's own fork (the Terminals page's terminal), so `/proc/self/exe`,
/// the running image even when deleted, works too; a terminal app's child
/// would find the terminal there instead.
pub(crate) fn own_executable(this_process: bool) -> PathBuf {
    pick_executable(std::env::current_exe().ok(), this_process, Path::is_file)
}

fn pick_executable(
    current: Option<PathBuf>,
    this_process: bool,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(current) = current {
        let live = current
            .to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
            .map_or_else(|| current.clone(), PathBuf::from);
        if exists(&live) {
            return live;
        }
    }
    let running = Path::new("/proc/self/exe");
    if this_process && exists(running) {
        return running.to_path_buf();
    }
    PathBuf::from("desklink")
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

    /// After `cp` + `mv` replaced the binary, the running one reads
    /// "(deleted)": the new file at its path runs, else the running image
    /// (own children only), else `desklink` on PATH.
    #[test]
    fn a_replaced_binary_still_starts_its_clients() {
        let deleted = Some(PathBuf::from("/home/sam/.local/bin/desklink (deleted)"));
        let everything = |_: &Path| true;
        let nothing = |_: &Path| false;
        let only_proc = |path: &Path| path == Path::new("/proc/self/exe");
        assert_eq!(
            pick_executable(deleted.clone(), true, everything),
            PathBuf::from("/home/sam/.local/bin/desklink")
        );
        assert_eq!(
            pick_executable(deleted.clone(), true, only_proc),
            PathBuf::from("/proc/self/exe")
        );
        assert_eq!(
            pick_executable(deleted.clone(), false, only_proc),
            PathBuf::from("desklink"),
            "a terminal app's child must not run the terminal"
        );
        assert_eq!(
            pick_executable(None, true, nothing),
            PathBuf::from("desklink")
        );
        assert_eq!(
            pick_executable(Some("/usr/bin/desklink".into()), false, everything),
            PathBuf::from("/usr/bin/desklink")
        );
        // The real thing: this test binary exists under its own name.
        let me = own_executable(false);
        assert!(me.is_file() && !me.to_string_lossy().ends_with(" (deleted)"));
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
