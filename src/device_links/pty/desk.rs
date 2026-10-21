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

fn socket_path() -> PathBuf {
    let base = super::host::runtime_dir().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("desklink-{}", unsafe { libc::getuid() }))
    });
    base.join("desklink").join("terminal.sock")
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
    let path = socket_path();
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err()
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
                let _ = std::thread::Builder::new()
                    .name("desklink-desk-terminal".into())
                    .spawn(move || {
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
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let (id, rows, cols) = parse_attach(&line).ok_or("malformed attach request")?;
    let session = super::registry::registry()
        .lock()
        .map_err(|_| "terminal registry lock poisoned")?
        .get(&id)
        .ok_or("no such DeskLink terminal")?;
    // A desk window sets the size while it is open; the phone shows it
    // scaled. With several, the last one to attach, resize or type wins.
    let mut desk_size = (rows, cols);
    let _ = session.resize(rows, cols);
    session.touch();
    session.desk_window_opened();
    struct Closed<'a>(&'a PtySession);
    impl Drop for Closed<'_> {
        fn drop(&mut self) {
            self.0.desk_window_closed();
        }
    }
    let _closed = Closed(&session);

    let output = {
        let session = Arc::clone(&session);
        let stream = stream.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("desklink-desk-terminal-out".into())
            .spawn(move || pump_output(&session, stream))
            .map_err(|e| e.to_string())?
    };
    // Input from the desk's keyboard. The desk is this computer's own user,
    // so it types without the phone's single-writer lease.
    loop {
        match read_frame(&mut reader) {
            Ok((INPUT, bytes)) => {
                session.touch();
                if session.size() != desk_size {
                    let _ = session.resize(desk_size.0, desk_size.1);
                }
                if session.write_input(&bytes).is_err() {
                    break;
                }
            }
            Ok((RESIZE, payload)) if payload.len() == 4 => {
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                desk_size = (rows.clamp(1, 1000), cols.clamp(1, 1000));
                let _ = session.resize(desk_size.0, desk_size.1);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = output.join();
    Ok(())
}

fn pump_output(session: &PtySession, mut stream: UnixStream) {
    let mut offset = None;
    while let Ok(resync) = session.resync(offset, 64 * 1024) {
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

/// Opens a window on this desktop showing session `id`.
pub(crate) fn open_on_desktop(id: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let exe = exe.to_string_lossy().into_owned();
    let title = "DeskLink terminal";
    let client = super::host::attach_client(&exe, id);
    let client: Vec<&str> = client.iter().map(String::as_str).collect();
    let in_flatpak = super::host::in_flatpak();
    let attempts: [(&str, Vec<&str>); 7] = [
        ("kgx", [&["--title", title, "--"][..], &client].concat()),
        ("ptyxis", [&["--new-window", "--"][..], &client].concat()),
        (
            "gnome-terminal",
            [&["--title", title, "--"][..], &client].concat(),
        ),
        ("konsole", [&["-e"][..], &client].concat()),
        ("xdg-terminal-exec", client.to_vec()),
        ("kitty", client.to_vec()),
        ("xterm", [&["-e"][..], &client].concat()),
    ];
    for (program, args) in attempts {
        // Inside a Flatpak the terminal app is the host's. `flatpak-spawn`
        // itself always starts, so a missing host program shows up as a quick
        // failing exit instead of a spawn error.
        let mut command = if in_flatpak {
            let mut command = std::process::Command::new("flatpak-spawn");
            command.args(["--host", program]).args(&args);
            command
        } else {
            let mut command = std::process::Command::new(program);
            command.args(&args);
            command
        };
        if let Ok(mut child) = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if in_flatpak {
                std::thread::sleep(Duration::from_millis(800));
                if let Ok(Some(status)) = child.try_wait() {
                    if !status.success() {
                        continue;
                    }
                }
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
    let mut stream = match UnixStream::connect(socket_path()) {
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
    use super::*;

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
