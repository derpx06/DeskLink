//! A DeskLink terminal session shown at the desk as well as on the phone.
//!
//! When the phone starts a session, a terminal window opens on this computer
//! running `desklink --attach-terminal <id>`: a small client that joins the
//! *same* live session through a private local socket. Both screens show it,
//! and either keyboard types into it, so work carries on at the desk and back.
//! Nothing extra is installed; DeskLink hosts the sessions itself, so they last
//! while DeskLink runs.
//!
//! The socket lives in the user's runtime directory with owner-only
//! permissions, and every connection's peer credentials must be this user —
//! another account on the machine cannot reach a shell through it.
//!
//! Wire format, both directions: `[kind: u8][len: u32 BE][payload]`.
//! Client → host: `INPUT` bytes, `RESIZE` (rows u16, cols u16).
//! Host → client: `OUTPUT` bytes, `EXIT` (status u32).
//! The client's first line is `ATTACH <id> <rows> <cols>\n`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::session::{PtySession, Resync};

const INPUT: u8 = 0;
const RESIZE: u8 = 1;
const OUTPUT: u8 = 2;
const EXIT: u8 = 3;
/// Largest frame either side accepts.
const MAX_FRAME: usize = 256 * 1024;

/// Where the sessions are served: under the user's runtime directory, else the
/// user's own cache directory. Never a shared place such as `/tmp`, where
/// another user could have made the directory first.
fn socket_path() -> Option<PathBuf> {
    let base = super::host::runtime_dir().or_else(dirs::cache_dir)?;
    Some(base.join("desklink").join("terminal.sock"))
}

/// Whether `path` is a real (not linked) entry owned by this user.
fn owned_by_us(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| !meta.file_type().is_symlink() && meta.uid() == unsafe { libc::getuid() })
}

/// Desk clients served at once; a window per session is a handful, never this many.
const MAX_CLIENTS: usize = 32;
/// The attach line is short; the peer has this long to send it.
const ATTACH_LINE_MAX: u64 = 256;
const ATTACH_LINE_WITHIN: Duration = Duration::from_secs(5);
static CLIENTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ClientSlot;

impl ClientSlot {
    fn claim() -> Option<Self> {
        use std::sync::atomic::Ordering;
        if CLIENTS.fetch_add(1, Ordering::AcqRel) < MAX_CLIENTS {
            Some(Self)
        } else {
            CLIENTS.fetch_sub(1, Ordering::AcqRel);
            None
        }
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        CLIENTS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(crate) fn encode(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn read_frame(reader: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("DeskLink terminal frame too large"));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// `ATTACH <id> <rows> <cols>` → the parts, bounded like the phone protocol.
pub(crate) fn parse_attach(line: &str) -> Option<(String, u16, u16)> {
    let mut parts = line.trim_end().split(' ');
    if parts.next()? != "ATTACH" {
        return None;
    }
    let id = parts.next()?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let rows = parts.next()?.parse::<u16>().ok()?.clamp(1, 1000);
    let cols = parts.next()?.parse::<u16>().ok()?.clamp(1, 1000);
    parts.next().is_none().then(|| (id.to_string(), rows, cols))
}

// ---- host (inside DeskLink) ------------------------------------------------

/// Starts accepting desk clients. Called once, at startup.
pub(crate) fn start_server() {
    // The login-shell probe can take a moment; pay it now, not in the first
    // session list the phone asks for. `installed` starts its own refresh in
    // the background, so the first list already has launchers.
    let _ = thread::Builder::new()
        .name("desklink-login-path".into())
        .spawn(|| {
            super::launchers::login_path();
            super::launchers::installed();
        });
    let Some(path) = socket_path() else {
        eprintln!("[DeskLink] Shared terminals at the desk are unavailable: no private directory");
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err()
        || !owned_by_us(dir)
        || std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err()
    {
        eprintln!(
            "[DeskLink] Shared terminals at the desk are unavailable: no private runtime directory"
        );
        return;
    }
    // Another DeskLink answering here owns the sessions; never take its
    // socket. One nobody answers on is left over from a crash: replace it.
    if UnixStream::connect(&path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[DeskLink] Shared terminals at the desk are unavailable: {error}");
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = std::thread::Builder::new()
        .name("desklink-desk-terminals".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if !same_user(&stream) {
                    continue;
                }
                let Some(slot) = ClientSlot::claim() else {
                    continue; // dropped: closes the connection
                };
                let _ = std::thread::Builder::new()
                    .name("desklink-desk-terminal".into())
                    .spawn(move || {
                        let _slot = slot;
                        if let Err(error) = serve(stream) {
                            eprintln!("[DeskLink] Desk terminal ended: {error}");
                        }
                    });
            }
        });
}

/// The kernel's word on who connected: only this user may reach a shell.
fn same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut len,
        )
    } == 0;
    ok && credentials.uid == unsafe { libc::getuid() }
}

fn serve(stream: UnixStream) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    // The request is one short line, sent at once: a client that does not send
    // it (or never ends it) is not kept waiting. The timeout is lifted for the
    // session itself, which is idle for as long as nobody types.
    stream
        .set_read_timeout(Some(ATTACH_LINE_WITHIN))
        .map_err(|e| e.to_string())?;
    std::io::Read::by_ref(&mut reader)
        .take(ATTACH_LINE_MAX)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;
    let (id, rows, cols) = parse_attach(&line).ok_or("malformed attach request")?;
    let session = super::registry::registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned")?
        .get(&id)
        .ok_or("no such DeskLink terminal")?;
    // A desk window sets the size while it is open, unless the phone that made
    // the session (or the person, on the phone) claimed it: then the window
    // shows the program top-left until the desk is resized or typed in. With
    // several windows, the last one to resize or type wins.
    let mut desk_size = Some((rows, cols));
    session.touch();
    session.desk_window_opened();
    session.desk_window_size(rows, cols, false);
    struct Closed<'a>(&'a PtySession);
    impl Drop for Closed<'_> {
        fn drop(&mut self) {
            self.0.desk_window_closed();
        }
    }
    let _closed = Closed(&session);

    // The pump only notices a closed socket when it writes, which an idle
    // shell never makes it do: it is told to stop instead.
    let stop = Arc::new(AtomicBool::new(false));
    let output = {
        let session = Arc::clone(&session);
        let stream = stream.try_clone().map_err(|e| e.to_string())?;
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("desklink-desk-terminal-out".into())
            .spawn(move || pump_output(&session, stream, &stop))
            .map_err(|e| e.to_string())?
    };
    // Input from the desk's keyboard. The desk is this computer's own user,
    // so it types without the phone's single-writer lease.
    loop {
        match read_frame(&mut reader) {
            Ok((INPUT, bytes)) => {
                session.touch();
                // Typing at the desk takes the size back from the phone.
                if let Some((rows, cols)) = desk_size {
                    session.desk_window_size(rows, cols, true);
                }
                if session.write_input(&bytes).is_err() {
                    break;
                }
            }
            Ok((RESIZE, payload)) if payload.len() == 4 => {
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                desk_size = Some((rows.clamp(1, 1000), cols.clamp(1, 1000)));
                session.desk_window_size(rows, cols, true);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Release);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = output.join();
    Ok(())
}

fn pump_output(session: &PtySession, mut stream: UnixStream, stop: &AtomicBool) {
    let mut offset = None;
    while !stop.load(Ordering::Acquire) {
        let Ok(resync) = session.resync(offset, 64 * 1024) else {
            return;
        };
        let (next, data) = match resync {
            Resync::Snapshot { at, data } => (at, data),
            Resync::Bytes { from, data } => (from + data.len() as u64, data),
        };
        offset = Some(next);
        if !data.is_empty() {
            if stream.write_all(&encode(OUTPUT, &data)).is_err() {
                return;
            }
            continue;
        }
        if let Some(status) = session.finished() {
            let _ = stream.write_all(&encode(EXIT, &status.to_be_bytes()));
            return;
        }
        std::thread::sleep(Duration::from_millis(12));
    }
}

/// Starts a session at the desk and opens its window; the phone lists it too.
pub(crate) fn new_desk_session() -> Result<String, String> {
    let info = super::registry::registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned".to_string())?
        .create(None, 24, 80, DESK_OWNER)?;
    open_on_desktop(&info.id);
    Ok(info.id)
}

/// [`new_desk_session`] in `folder` (`~/api`, `/srv`), which must be an
/// existing readable directory.
pub(crate) fn new_desk_session_in(folder: &str) -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or("No home folder")?;
    let cwd = super::registry::resolve_cwd(folder, &home).ok_or("That folder is not available")?;
    let info = super::registry::registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned".to_string())?
        .create_in(None, 24, 80, DESK_OWNER, Some(&cwd), None)?;
    open_on_desktop(&info.id);
    Ok(info.id)
}

/// The registry owner of sessions started at this computer.
const DESK_OWNER: &str = "desk";

/// Opens a window on this desktop showing session `id`. Trying terminals
/// waits on each (a failed one exits at once), so it runs off the caller's
/// thread, which is often the UI's.
pub(crate) fn open_on_desktop(id: &str) {
    let id = id.to_string();
    let _ = thread::Builder::new()
        .name("desklink-terminal-open".into())
        .spawn(move || open_blocking(&id));
}

fn open_blocking(id: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.to_string_lossy().into_owned();
    let title = "DeskLink terminal";
    let client = super::host::attach_client(&exe, id);
    let in_flatpak = super::host::in_flatpak();
    let attempts = terminal_attempts(&client, title, std::env::var("TERMINAL").ok().as_deref());
    for (program, args) in attempts {
        // Inside a Flatpak the terminal app is the host's. `flatpak-spawn`
        // itself always starts, so a missing host program shows up as a quick
        // failing exit instead of a spawn error.
        let mut command = if in_flatpak {
            let mut command = std::process::Command::new("flatpak-spawn");
            command.args(["--host", &program]).args(&args);
            command
        } else {
            let mut command = std::process::Command::new(&program);
            command.args(&args);
            command
        };
        if let Ok(mut child) = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            // A program that starts and at once exits with an error did not
            // open a window (a broken `$TERMINAL`, a missing host program in
            // a Flatpak, a terminal that cannot reach its server): try the
            // next one. Exiting cleanly is a launcher that handed over.
            if exits_with_error_soon(&mut child, STARTED_WITHIN) {
                continue;
            }
            let _ = std::thread::Builder::new()
                .name("desklink-terminal-window".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return;
        }
    }
    eprintln!("[DeskLink] No terminal app found to show the shared session on the desktop");
}

/// Terminal programs and how each is told to run a command: the arguments
/// that come before it (`{title}` is the window title). `x-terminal-emulator`
/// is Debian's alternatives link, the user's choice on Debian and Ubuntu.
const TERMINALS: [(&str, &[&str]); 20] = [
    // The first seven are the original order, unchanged: a GNOME or KDE
    // machine picks exactly what it always did.
    ("kgx", &["--title", "{title}", "--"]),
    ("ptyxis", &["--new-window", "--"]),
    ("gnome-terminal", &["--title", "{title}", "--"]),
    ("konsole", &["-e"]),
    ("xdg-terminal-exec", &[]),
    // Other desktops' own terminals, before the general-purpose ones.
    ("xfce4-terminal", &["--title", "{title}", "-x"]),
    ("mate-terminal", &["--title", "{title}", "-x"]),
    ("lxterminal", &["--title={title}", "-e"]),
    ("qterminal", &["-e"]),
    ("x-terminal-emulator", &["-e"]),
    ("kitty", &["--title", "{title}"]),
    ("alacritty", &["--title", "{title}", "-e"]),
    ("foot", &["--title", "{title}"]),
    ("wezterm", &["start", "--"]),
    ("terminator", &["--title", "{title}", "-x"]),
    ("urxvt", &["-title", "{title}", "-e"]),
    ("st", &["-t", "{title}", "-e"]),
    ("sakura", &["--title", "{title}", "-x"]),
    ("cool-retro-term", &["-e"]),
    ("xterm", &["-T", "{title}", "-e"]),
];
// Not listed: tilix, termite and deepin-terminal. Their `-e` takes one string
// to hand to a shell, not an argument list, so the attach command would need
// shell quoting that has not been checked against their documentation. Only
// the first five and xterm have been used on a live desktop; the rest follow
// each program's documented "run this command" flag and are unverified.

/// The commands to try, in order, to show the session in a window: the
/// user's `$TERMINAL` first (as `terminal -e command`, the common convention),
/// then [`TERMINALS`]. Each is `(program, arguments)` with `client` last.
fn terminal_attempts(
    client: &[String],
    title: &str,
    preferred: Option<&str>,
) -> Vec<(String, Vec<String>)> {
    let mut attempts = Vec::new();
    if let Some(program) = preferred.filter(|name| {
        // A bare program name only: `$TERMINAL` is never run through a shell.
        !name.is_empty() && !name.contains(char::is_whitespace) && !name.contains('/')
    }) {
        if !TERMINALS.iter().any(|(known, _)| *known == program) {
            attempts.push((
                program.to_string(),
                ["-e".to_string()]
                    .into_iter()
                    .chain(client.iter().cloned())
                    .collect(),
            ));
        }
    }
    for (program, before) in TERMINALS {
        let mut args: Vec<String> = before
            .iter()
            .map(|arg| arg.replace("{title}", title))
            .collect();
        args.extend(client.iter().cloned());
        let entry = (program.to_string(), args);
        // The user's own choice, when it is one we know, goes first.
        if preferred == Some(program) {
            attempts.insert(0, entry);
        } else {
            attempts.push(entry);
        }
    }
    attempts
}

/// How long a terminal has to stay up (or exit cleanly) to count as opened.
const STARTED_WITHIN: Duration = Duration::from_millis(800);

/// Waits up to `within` for `child`; `true` when it exited with a failure in
/// that time. Still running, or exited successfully, is `false`.
fn exits_with_error_soon(child: &mut std::process::Child, within: Duration) -> bool {
    let deadline = std::time::Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return !status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            _ => return false,
        }
    }
}

/// The terminal programs found on `PATH` (for the system check).
pub(crate) fn installed_terminals() -> std::collections::BTreeSet<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    TERMINALS
        .iter()
        .map(|(program, _)| *program)
        .filter(|program| {
            std::env::split_paths(&path).any(|dir| {
                dir.join(program)
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
        })
        .map(str::to_string)
        .collect()
}

// ---- client (`desklink --attach-terminal <id>`, inside the desk window) -----

/// Puts the controlling terminal back as it was, however the client ends.
struct RawMode(libc::termios);

impl RawMode {
    fn enable() -> Option<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
        Some(Self(original))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

fn window_size() -> (u16, u16) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_row > 0
    {
        (size.ws_row, size.ws_col)
    } else {
        (24, 80)
    }
}

/// Runs the desk client until the session ends or the window closes.
pub(crate) fn run_client(id: &str) -> i32 {
    // A socket that is not ours (or a link to one) is not a DeskLink to type into.
    let Some(path) = socket_path().filter(|path| owned_by_us(path)) else {
        eprintln!("DeskLink is not running.");
        return 1;
    };
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("DeskLink is not running ({error}).");
            return 1;
        }
    };
    let (rows, cols) = window_size();
    if stream
        .write_all(format!("ATTACH {id} {rows} {cols}\n").as_bytes())
        .is_err()
    {
        return 1;
    }
    let raw = RawMode::enable();

    // Keyboard → session, and window size changes (polled: no signal handler
    // to get right, and a quarter second is unnoticeable for a resize).
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return 1,
    };
    let _ = std::thread::Builder::new()
        .name("desk-in".into())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 4096];
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&encode(INPUT, &buffer[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    if let Ok(mut resizer) = stream.try_clone() {
        let _ = std::thread::Builder::new()
            .name("desk-size".into())
            .spawn(move || {
                let mut last = window_size();
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    let now = window_size();
                    if now != last {
                        last = now;
                        let mut payload = now.0.to_be_bytes().to_vec();
                        payload.extend_from_slice(&now.1.to_be_bytes());
                        if resizer.write_all(&encode(RESIZE, &payload)).is_err() {
                            break;
                        }
                    }
                }
            });
    }

    // Session → screen.
    let mut stdout = std::io::stdout();
    let status = loop {
        match read_frame(&mut stream) {
            Ok((OUTPUT, bytes)) => {
                let _ = stdout.write_all(&bytes);
                let _ = stdout.flush();
            }
            Ok((EXIT, payload)) if payload.len() == 4 => {
                break u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as i32;
            }
            Ok(_) => {}
            Err(_) => break 0,
        }
    };
    drop(raw);
    println!("\r\n[DeskLink terminal session ended]");
    status
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_terminal_gets_the_attach_command_last_and_a_titled_window() {
        let client: Vec<String> = ["/usr/bin/desklink", "--attach-terminal", "7"]
            .map(String::from)
            .to_vec();
        let attempts = super::terminal_attempts(&client, "DeskLink terminal", None);
        assert_eq!(attempts.len(), super::TERMINALS.len());
        for (program, args) in &attempts {
            assert!(args.ends_with(&client), "{program}: {args:?}");
            assert!(!args.iter().any(|arg| arg.contains("{title}")), "{program}");
        }
        // The terminals the common desktops ship are all there.
        for program in [
            "kgx",
            "konsole",
            "xfce4-terminal",
            "mate-terminal",
            "x-terminal-emulator",
            "foot",
            "alacritty",
            "kitty",
            "xterm",
        ] {
            assert!(
                attempts.iter().any(|(name, _)| name == program),
                "{program}"
            );
        }
        let xfce = attempts
            .iter()
            .find(|(name, _)| name == "xfce4-terminal")
            .unwrap();
        assert_eq!(xfce.1[..3], ["--title", "DeskLink terminal", "-x"]);
    }

    #[test]
    fn the_original_terminals_keep_their_order_and_gnome_still_starts_with_kgx() {
        let client = vec!["desklink".to_string()];
        let order: Vec<String> = super::terminal_attempts(&client, "t", None)
            .into_iter()
            .map(|(program, _)| program)
            .collect();
        let at = |name: &str| order.iter().position(|p| p == name).unwrap();
        let original = [
            "kgx",
            "ptyxis",
            "gnome-terminal",
            "konsole",
            "xdg-terminal-exec",
            "kitty",
            "xterm",
        ];
        for pair in original.windows(2) {
            assert!(at(pair[0]) < at(pair[1]), "{pair:?}");
        }
        assert_eq!(&order[..5], &original[..5]);
        // Shells' one-string `-e` terminals are not in the list.
        for left_out in ["tilix", "termite", "deepin-terminal"] {
            assert!(!order.iter().any(|p| p == left_out), "{left_out}");
        }
    }

    #[test]
    fn a_terminal_that_exits_with_an_error_at_once_counts_as_failed() {
        let start = |script: &str| {
            std::process::Command::new("sh")
                .args(["-c", script])
                .spawn()
                .unwrap()
        };
        let within = std::time::Duration::from_millis(800);
        assert!(super::exits_with_error_soon(&mut start("exit 3"), within));
        assert!(!super::exits_with_error_soon(&mut start("exit 0"), within));
        let mut running = start("sleep 5");
        assert!(!super::exits_with_error_soon(
            &mut running,
            std::time::Duration::from_millis(100)
        ));
        let _ = running.kill();
        let _ = running.wait();
    }

    #[test]
    fn the_users_terminal_is_tried_first_and_never_through_a_shell() {
        let client = vec!["desklink".to_string(), "--attach-terminal".to_string()];
        let first = |preferred| {
            super::terminal_attempts(&client, "t", preferred)
                .into_iter()
                .next()
                .map(|(program, _)| program)
        };
        assert_eq!(first(Some("foot")).as_deref(), Some("foot"));
        assert_eq!(first(Some("wezterm")).as_deref(), Some("wezterm"));
        assert_eq!(first(Some("someterm")).as_deref(), Some("someterm"));
        assert_eq!(first(None).as_deref(), Some("kgx"));
        for hostile in ["", "rm -rf ~", "/tmp/x", "a b"] {
            assert_eq!(first(Some(hostile)).as_deref(), Some("kgx"), "{hostile:?}");
        }
        let unknown = super::terminal_attempts(&client, "t", Some("someterm"));
        assert_eq!(unknown[0].1[0], "-e");
    }

    use super::*;

    #[test]
    fn the_socket_is_never_under_a_shared_directory() {
        assert!(socket_path().is_some(), "a runtime or cache directory");
        let source = include_str!("desk.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(
            !source.contains("temp_dir()"),
            "no fallback to a world-writable directory"
        );
    }

    #[test]
    fn only_a_real_entry_of_ours_is_trusted() {
        let dir =
            std::env::temp_dir().join(format!("desklink-own-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(owned_by_us(&dir));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(!owned_by_us(&link), "a link is not the entry itself");
        assert!(!owned_by_us(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clients_are_capped_and_the_slot_comes_back() {
        let held: Vec<_> = (0..MAX_CLIENTS)
            .map(|_| ClientSlot::claim().expect("free"))
            .collect();
        assert!(ClientSlot::claim().is_none(), "the cap is firm");
        assert!(
            ClientSlot::claim().is_none(),
            "and a refusal does not leak a count"
        );
        drop(held);
        assert!(ClientSlot::claim().is_some(), "free again once they end");
    }

    /// A quiet shell never makes the pump write, so only a stop flag ends it:
    /// the window count must fall when the desk window closes.
    #[test]
    fn closing_the_window_of_a_quiet_session_ends_the_attachment() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let info = super::super::registry::registry()
            .lock()
            .unwrap()
            .create(Some(command), 24, 80, "desk-test")
            .expect("quiet session");
        let session = super::super::registry::registry()
            .lock()
            .unwrap()
            .get(&info.id)
            .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = std::thread::spawn(move || serve(server));
        client
            .write_all(format!("ATTACH {} 30 100\n", info.id).as_bytes())
            .unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !session.desk_sets_size() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(session.desk_sets_size(), "the window attached");
        drop(client);
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while !served.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(served.is_finished(), "serve must return on a quiet session");
        assert!(!session.desk_sets_size(), "the desk window count falls");
    }

    /// The phone that made a session keeps its size when the desk window opens
    /// for it; the first thing typed at the desk takes the size back.
    #[test]
    fn a_window_opening_over_a_phones_claim_leaves_its_size_until_typed_in() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let info = super::super::registry::registry()
            .lock()
            .unwrap()
            .create(Some(command), 24, 80, "desk-test")
            .expect("quiet session");
        let session = super::super::registry::registry()
            .lock()
            .unwrap()
            .get(&info.id)
            .unwrap();
        session.claim_for_phone("phone-1", 30, 50).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = std::thread::spawn(move || serve(server));
        client
            .write_all(format!("ATTACH {} 40 120\n", info.id).as_bytes())
            .unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !session.has_desk_window() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(session.has_desk_window());
        assert_eq!(session.size(), (30, 50), "the window did not squeeze it");
        assert!(!session.desk_sets_size() && session.phone_sets_size());

        client.write_all(&encode(INPUT, b"x")).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while session.phone_sets_size() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session.size(), (40, 120), "typing at the desk took it back");
        assert!(session.desk_sets_size());
        drop(client);
        let _ = served.join();
        assert!(!session.has_desk_window());
    }

    /// The desk taking the size back is one step: whoever sees the phone's
    /// claim gone sees the desk's size already in place. (The claim used to be
    /// cleared first and the PTY resized after, so a reader that waited for
    /// the release and then read the size could catch the old one.)
    #[test]
    fn the_claim_is_never_seen_released_before_the_size_is_back() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let info = super::super::registry::registry()
            .lock()
            .unwrap()
            .create(Some(command), 24, 80, "desk-test")
            .expect("quiet session");
        let session = super::super::registry::registry()
            .lock()
            .unwrap()
            .get(&info.id)
            .unwrap();
        for round in 0..300 {
            session.claim_for_phone("phone-1", 30, 50).unwrap();
            assert_eq!(session.size(), (30, 50));
            let watcher = {
                let session = Arc::clone(&session);
                std::thread::spawn(move || {
                    while session.phone_sets_size() {
                        std::thread::yield_now();
                    }
                    session.size()
                })
            };
            session.desk_window_size(40, 120, true);
            assert_eq!(
                watcher.join().unwrap(),
                (40, 120),
                "round {round}: the claim was seen released with the phone's size"
            );
        }
    }

    #[test]
    fn an_attach_line_that_never_ends_is_cut_not_buffered() {
        let (client, server) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let mut client = client;
            // Far more than the line bound, and no newline.
            let _ = client.write_all(&[b'A'; 64 * 1024]);
            client
        });
        let result = serve(server);
        assert!(
            result.is_err(),
            "a line past its bound is a malformed request"
        );
        let _ = writer.join();
    }

    #[test]
    fn attach_requests_are_strictly_parsed() {
        assert_eq!(
            parse_attach("ATTACH abc-1 40 120\n"),
            Some(("abc-1".into(), 40, 120))
        );
        assert_eq!(
            parse_attach("ATTACH abc 0 5000"),
            Some(("abc".into(), 1, 1000))
        );
        assert_eq!(parse_attach("ATTACH ../x 40 120"), None);
        assert_eq!(parse_attach("ATTACH abc 40"), None);
        assert_eq!(parse_attach("ATTACH abc 40 120 extra"), None);
        assert_eq!(parse_attach("DETACH abc 40 120"), None);
    }

    #[test]
    fn frames_round_trip_and_oversized_frames_are_refused() {
        let frame = encode(INPUT, b"ls\r");
        assert_eq!(
            read_frame(&mut frame.as_slice()).unwrap(),
            (INPUT, b"ls\r".to_vec())
        );
        let mut huge = vec![OUTPUT];
        huge.extend_from_slice(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }
}
