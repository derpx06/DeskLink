//! One PTY session: a real shell on this machine, outliving any one client.
//!
//! A session is deliberately **not** tied to a phone connection. The reader
//! thread keeps draining the PTY whether or not anyone is attached, because the
//! entire point of the feature is that a build keeps building while the phone
//! is in a pocket.
//!
//! Two consumers of every byte, both fed on every read:
//!
//! * an [`OutputRing`] — byte-exact replay for a short absence, so a client
//!   that blinked keeps its scrollback;
//! * a [`vt100::Parser`] — a screen model, so a client that was away for
//!   twenty minutes gets the *current screen* immediately instead of a
//!   megabyte of replay it would have to render to catch up.
//!
//! The snapshot path costs O(screen), not O(absence). That is the whole reason
//! the parser is here.
//!
//! Input goes the other way through a bounded queue and a writer thread of its
//! own, so a program that is not reading its terminal can never stall the
//! caller — see [`PtySession::write_input`].

use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use uuid::Uuid;

use super::launchers;
use super::ring::{OutputRing, RING_BYTES};

/// Bytes taken from the PTY per read. Matches what sshx and RustDesk use; large
/// enough that a flood is not a syscall storm, small enough to stay responsive.
const READ_CHUNK: usize = 4 * 1024;

/// Clamp for client-supplied geometry. A terminal cannot be zero-sized, and an
/// absurd one would make the screen model allocate without bound — dimensions
/// arrive from the network and are not trusted.
const MIN_DIMENSION: u16 = 1;
const MAX_DIMENSION: u16 = 1000;

/// Input chunks that may wait for the writer thread. Each is at most one wire
/// frame (16 KiB), so this bounds a stuck session at about 1 MiB of queued
/// keystrokes; beyond that input is refused rather than buffered without limit.
const INPUT_QUEUE_CHUNKS: usize = 64;

/// How long a finished shell's output may still be in flight before the
/// session counts as finished anyway. Normally the reader sees EOF as soon as
/// the last process holding the slave exits; a background job that outlives the
/// shell keeps the slave open, and the phone should not wait for it.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(300);

/// Grace between hanging a closing session up and killing what ignored it.
const CLOSE_GRACE: Duration = Duration::from_millis(300);

/// How long a closing session's leader is waited for after `SIGKILL`.
const REAP_PATIENCE: Duration = Duration::from_secs(2);

/// Longest session title. The title is derived from a client-supplied argv[0]
/// and is repeated in every session list, so it must not be a way to inflate one.
pub(crate) const MAX_TITLE_CHARS: usize = 64;

/// Environment a phone-driven shell may inherit from DeskLink.
///
/// An allow-list, because the deny-list version of this is a leak waiting for
/// the next `DESKLINK_*` variable: DeskLink is started with TURN credentials
/// and other deployment inputs a terminal has no business reading. What is here
/// is what a login shell and GUI-launching tools need to behave normally.
const ALLOWED_ENVIRONMENT: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "LANG",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// What a client needs to render after attaching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resync {
    /// The client's offset is still in the ring: exact bytes, scrollback intact.
    Bytes { from: u64, data: Vec<u8> },
    /// Too much happened: reset and repaint from the screen model.
    Snapshot { at: u64, data: Vec<u8> },
}

/// Output state shared between the reader thread and every client.
///
/// No `Debug`: `vt100::Parser` does not implement it, and a screen model is
/// not something to dump into a log anyway — it is terminal contents.
struct Output {
    ring: OutputRing,
    parser: vt100::Parser,
    /// Last time anything happened to this session — output, or a client
    /// attaching, typing or leaving. The idle reaper reads it.
    last_activity: Instant,
}

impl Output {
    /// Escape codes that reproduce the current screen on a fresh client.
    ///
    /// `\x1bc` (RIS) first, so we are not painting onto whatever the client had
    /// before — a reattach must not inherit a half-finished screen.
    ///
    /// The `?1049h` wrapper is ours, not `vt100`'s: `state_formatted` writes
    /// only the *active* grid and never announces that it is the alternate one.
    /// Without this, attaching while `vim` is open paints vim's screen into the
    /// client's normal buffer, and quitting vim then emits `?1049l` and drops
    /// the client into a buffer that never had the shell in it.
    fn snapshot(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = Vec::from(&b"\x1bc"[..]);
        if screen.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        out.extend_from_slice(&screen.state_formatted());
        out
    }
}

pub(crate) struct PtySession {
    id: String,
    title: String,
    /// Behind a lock like the child: `MasterPty` is `Send` but not `Sync`, and
    /// the registry hands out `Arc<PtySession>` to several clients.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Keystrokes waiting for the writer thread. Bounded, and only ever
    /// `try_send`: the caller is the peer worker thread.
    input: SyncSender<Vec<u8>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    /// Pid of the session leader, which is also the session and process-group id
    /// because the child called `setsid`.
    pid: Option<u32>,
    output: Arc<Mutex<Output>>,
    /// `ring.head()` published lock-free, so a pump that finds nothing new can
    /// say so without taking the output lock.
    head: Arc<AtomicU64>,
    /// The reader thread has seen EOF: every byte the program wrote is in the
    /// ring.
    eof: Arc<AtomicBool>,
    /// Set once by [`Self::close`]; a deliberate end, as opposed to the shell
    /// exiting on its own.
    closed: AtomicBool,
    exited_seen: OnceLock<Instant>,
    size: Mutex<(u16, u16)>,
    /// Terminal windows open on it at the desk. While there is one, it alone
    /// sets the size: a phone's keyboard or layout changing kept squeezing the
    /// desk window to the phone's shape.
    desk_windows: AtomicU64,
    /// When it was started, for the desk's terminal details.
    started: std::time::SystemTime,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession")
            .field("id", &self.id)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

impl PtySession {
    /// Spawns `command` on a new PTY. Pass `None` for the user's login shell.
    ///
    /// Either way the program runs with the allow-listed environment and starts
    /// in `$HOME`; see [`restricted`].
    #[cfg(test)]
    pub(crate) fn spawn(
        command: Option<CommandBuilder>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, String> {
        Self::spawn_in(command, rows, cols, None, None)
    }

    /// [`Self::spawn`] starting in `cwd` (already checked to be a readable
    /// directory) and listed as `title`, when given.
    pub(crate) fn spawn_in(
        command: Option<CommandBuilder>,
        rows: u16,
        cols: u16,
        cwd: Option<&std::path::Path>,
        title: Option<&str>,
    ) -> Result<Self, String> {
        let (rows, cols) = clamp_size(rows, cols);
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("Could not open a DeskLink terminal: {error}"))?;

        // `new_default_prog` resolves the login shell the way a terminal
        // emulator does: $SHELL if it is executable, else the password
        // database, with `-basename` as argv[0] so profile scripts run and the
        // user's PATH additions actually exist.
        let mut command = restricted(command.unwrap_or_else(CommandBuilder::new_default_prog));

        let title = title.map_or_else(
            || describe(&command),
            |title| title.chars().take(MAX_TITLE_CHARS).collect(),
        );
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        }
        // In a Flatpak, the shell and any program run on the host instead.
        if super::host::in_flatpak() {
            let argv = if command.is_default_prog() {
                super::host::host_login_shell()
            } else {
                super::host::host_command(command.get_argv())
            };
            let env: Vec<(OsString, OsString)> = command
                .iter_full_env_as_str()
                .map(|(key, value)| (key.into(), value.into()))
                .collect();
            let mut argv = argv;
            // `flatpak-spawn --host` starts in the host's $HOME otherwise.
            if let Some(cwd) = cwd {
                argv.insert(2, format!("--directory={}", cwd.display()).into());
            }
            command = CommandBuilder::from_argv(argv);
            for (key, value) in env {
                command.env(key, value);
            }
        } else if !command.is_default_prog() {
            // A bare name ("claude") runs from the login shell's PATH, and the
            // program gets that PATH too, so it can find its own helpers.
            let program = command.get_argv().first().cloned().unwrap_or_default();
            if let (Some(resolved), Some(path)) =
                (launchers::resolve(&program), launchers::login_path())
            {
                command.get_argv_mut()[0] = resolved.into_os_string();
                command.env("PATH", path);
            }
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("Could not start the DeskLink terminal: {error}"))?;
        let pid = child.process_id();
        // The slave is the child's now. Holding our copy would keep the master
        // readable forever after the shell exits, so the reader thread would
        // never see EOF and the session would never be reaped.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("Could not read the DeskLink terminal: {error}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("Could not write to the DeskLink terminal: {error}"))?;

        let output = Arc::new(Mutex::new(Output {
            ring: OutputRing::new(RING_BYTES),
            parser: vt100::Parser::new(rows, cols, 0),
            last_activity: Instant::now(),
        }));
        let head = Arc::new(AtomicU64::new(0));
        let eof = Arc::new(AtomicBool::new(false));
        let session = Self {
            id: Uuid::new_v4().to_string(),
            title,
            master: Mutex::new(pair.master),
            input: spawn_writer(writer),
            child: Arc::new(Mutex::new(child)),
            pid,
            output: Arc::clone(&output),
            head: Arc::clone(&head),
            eof: Arc::clone(&eof),
            closed: AtomicBool::new(false),
            exited_seen: OnceLock::new(),
            size: Mutex::new((rows, cols)),
            desk_windows: AtomicU64::new(0),
            started: std::time::SystemTime::now(),
        };
        spawn_reader(reader, output, head, eof);
        Ok(session)
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn started(&self) -> std::time::SystemTime {
        self.started
    }

    /// The shell's (session leader's) pid.
    pub(crate) fn shell_pid(&self) -> Option<i32> {
        self.pid.and_then(|pid| i32::try_from(pid).ok())
    }

    /// The process group in the terminal's foreground: the shell itself at a
    /// prompt, a job while one runs.
    pub(crate) fn foreground_pgrp(&self) -> Option<i32> {
        self.master.lock().ok()?.process_group_leader()
    }

    /// The foreground process's current directory (the shell's at a prompt),
    /// read from `/proc`. Unknown in a Flatpak: its sessions run on the host
    /// through `flatpak-spawn`, whose processes the sandbox's `/proc` does not
    /// show.
    pub(crate) fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.foreground_pgrp().or(self.shell_pid())?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The shell's program name ("bash", "zsh").
    pub(crate) fn shell_name(&self) -> Option<String> {
        process_name(self.shell_pid()?)
    }

    pub(crate) fn desk_window_opened(&self) {
        self.desk_windows.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn desk_window_closed(&self) {
        self.desk_windows.fetch_sub(1, Ordering::AcqRel);
    }

    /// Whether a desk window owns the size; a phone then views it scaled.
    pub(crate) fn desk_sets_size(&self) -> bool {
        self.desk_windows.load(Ordering::Acquire) > 0
    }

    pub(crate) fn size(&self) -> (u16, u16) {
        *self.size.lock().expect("DeskLink terminal size lock")
    }

    /// Queues client keystrokes for the PTY. **Never blocks.**
    ///
    /// A program that is not reading its terminal — a build, a `sleep`, a hung
    /// process — fills the kernel's input buffer after a few KiB, and a direct
    /// `write` then blocks. This runs on the peer worker thread with the bridge
    /// and registry locks held, so blocking here used to stall SDP, ICE, files
    /// and the handover for the whole peer. The bytes go to a writer thread
    /// through a bounded queue; when it is full the input is refused with an
    /// error the client can show, not buffered without limit.
    ///
    /// Control characters are written through untouched: `0x03` reaches the
    /// line discipline, which raises `SIGINT` in the *foreground process group*
    /// exactly as a physical terminal would. Nothing here interprets input —
    /// synthesising a signal ourselves would target the shell rather than
    /// whatever it is currently running.
    pub(crate) fn write_input(&self, bytes: &[u8]) -> Result<(), String> {
        match self.input.try_send(bytes.to_vec()) {
            Ok(()) => {
                self.touch();
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(
                "The program is not reading its input; DeskLink dropped what you typed".to_string(),
            ),
            Err(TrySendError::Disconnected(_)) => {
                Err("The DeskLink terminal is no longer accepting input".to_string())
            }
        }
    }

    /// Resizes the PTY and the screen model together.
    ///
    /// `resize` raises `SIGWINCH` for us; the parser must be told separately or
    /// the next snapshot would describe a screen of the old shape.
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> Result<(), String> {
        let (rows, cols) = clamp_size(rows, cols);
        self.master
            .lock()
            .map_err(|_| "DeskLink terminal master lock poisoned".to_string())?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("Could not resize the DeskLink terminal: {error}"))?;
        self.output
            .lock()
            .map_err(|_| "DeskLink terminal output lock poisoned".to_string())?
            .parser
            .screen_mut()
            .set_size(rows, cols);
        *self
            .size
            .lock()
            .map_err(|_| "DeskLink terminal size lock poisoned".to_string())? = (rows, cols);
        Ok(())
    }

    /// Nudges full-screen programs into repainting after a snapshot.
    ///
    /// The snapshot carries only what `vt100` models; images, hyperlinks and
    /// exotic underlines are not in it. One round-trip through a different
    /// geometry makes `vim`/`htop`/`tmux` redraw from their own state, which
    /// covers the gap for nothing more than two ioctls.
    pub(crate) fn nudge_redraw(&self) {
        let (rows, cols) = self.size();
        let widened = cols.saturating_add(1).min(MAX_DIMENSION);
        if widened == cols {
            return;
        }
        let _ = self.resize(rows, widened);
        let _ = self.resize(rows, cols);
    }

    /// One past the last output byte, without taking the output lock.
    pub(crate) fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// What to send a client attaching at `last_offset`.
    ///
    /// An offset the ring still holds gives at most `limit` bytes from it; the
    /// caller loops, advancing its offset, until it is level with
    /// [`Self::head`]. Anything the ring no longer holds gives a snapshot,
    /// which ignores `limit` — it is the whole screen or nothing.
    pub(crate) fn resync(&self, last_offset: Option<u64>, limit: usize) -> Result<Resync, String> {
        let output = self
            .output
            .lock()
            .map_err(|_| "DeskLink terminal output lock poisoned".to_string())?;
        if let Some(offset) = last_offset {
            if let Some(data) = output.ring.since(offset, limit) {
                return Ok(Resync::Bytes { from: offset, data });
            }
        }
        Ok(Resync::Snapshot {
            at: output.ring.head(),
            data: output.snapshot(),
        })
    }

    /// Records that a client did something with this session, so the idle
    /// reaper does not close one somebody is using.
    pub(crate) fn touch(&self) {
        if let Ok(mut output) = self.output.lock() {
            output.last_activity = Instant::now();
        }
    }

    /// Time since output, or a client last touched this session.
    pub(crate) fn idle_for(&self) -> Duration {
        self.output
            .lock()
            .map(|output| output.last_activity.elapsed())
            .unwrap_or_default()
    }

    /// Has the shell exited?
    pub(crate) fn exit_status(&self) -> Option<u32> {
        let status = self
            .child
            .lock()
            .ok()?
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.exit_code())?;
        self.exited_seen.get_or_init(Instant::now);
        Some(status)
    }

    /// The exit status, once the program has exited **and** everything it wrote
    /// has been read into the ring.
    ///
    /// Reporting an exit while the reader thread still has output in flight
    /// would have the client told the session ended before it saw the last
    /// line. Callers must read [`Self::head`] *after* this returns `Some`: the
    /// value is then final.
    pub(crate) fn finished(&self) -> Option<u32> {
        let status = self.exit_status()?;
        let seen = self.exited_seen.get().copied().unwrap_or_else(Instant::now);
        (self.eof.load(Ordering::Acquire) || seen.elapsed() >= EXIT_DRAIN_GRACE).then_some(status)
    }

    /// Whether [`Self::close`] has been called.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Ends the session deliberately, and everything the shell started with it.
    ///
    /// Hangs up the whole session first — a shell forwards `SIGHUP` to its jobs
    /// and a full-screen program saves its state — then kills whatever ignored
    /// it. The child is `setsid`, so its pid is the session id, and signalling
    /// by session catches background jobs a shell put in their own process
    /// group: killing only the direct child left them running, orphaned.
    ///
    /// Returns immediately; the escalation happens on a short-lived thread so
    /// a hung program cannot make the caller — usually holding the registry
    /// lock — wait.
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(session_id) = self.pid.and_then(|pid| i32::try_from(pid).ok()) else {
            if let Ok(mut child) = self.child.lock() {
                let _ = child.kill();
            }
            return;
        };
        // Only while the leader is unreaped is its pid certainly still ours.
        // Once its exit has been collected the number is free to be reused, and
        // signalling "its" session could hit an unrelated process. A shell that
        // exited on its own leaves its background jobs running, exactly as a
        // terminal emulator's `exit` would.
        if self.exited_seen.get().is_some() {
            return;
        }
        signal_session(session_id, libc::SIGHUP);
        let child = Arc::clone(&self.child);
        thread::spawn(move || {
            let deadline = Instant::now() + CLOSE_GRACE;
            while Instant::now() < deadline && !session_members(session_id).is_empty() {
                thread::sleep(Duration::from_millis(20));
            }
            signal_session(session_id, libc::SIGKILL);
            // Collect the leader, but never sit on the child lock indefinitely
            // for a process stuck in the kernel.
            let give_up = Instant::now() + REAP_PATIENCE;
            while Instant::now() < give_up {
                let reaped = child
                    .lock()
                    .map_or(true, |mut child| !matches!(child.try_wait(), Ok(None)));
                if reaped {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.close();
    }
}

/// Feeds queued keystrokes to the PTY on a thread of its own.
///
/// This is the only place a write to the master may block, and blocking here
/// only parks this thread. It ends when the session is dropped (the queue
/// closes) or the PTY refuses the write (the program is gone).
fn spawn_writer(mut writer: Box<dyn Write + Send>) -> SyncSender<Vec<u8>> {
    let (queue, incoming) = sync_channel::<Vec<u8>>(INPUT_QUEUE_CHUNKS);
    thread::spawn(move || {
        while let Ok(bytes) = incoming.recv() {
            if writer
                .write_all(&bytes)
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    queue
}

/// Drains the PTY forever, feeding both the ring and the screen model.
///
/// Deliberately a plain OS thread with a blocking read: this codebase has no
/// async runtime, sessions number in the single digits, and a blocking read is
/// both smaller and the shape `portable-pty` is built for — its master fd is
/// never set non-blocking.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output: Arc<Mutex<Output>>,
    head: Arc<AtomicU64>,
    eof: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buffer = [0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buffer) {
                // `portable-pty` maps the slave-closed `EIO` to a clean EOF, so
                // this is the normal end of a session, not an error.
                Ok(0) => break,
                Ok(count) => {
                    let Ok(mut output) = output.lock() else {
                        break;
                    };
                    output.parser.process(&buffer[..count]);
                    output.ring.push(&buffer[..count]);
                    output.last_activity = Instant::now();
                    head.store(output.ring.head(), Ordering::Release);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        eof.store(true, Ordering::Release);
    });
}

/// Signals every process in session `session_id`, and its process group.
///
/// The group is signalled first and atomically, so a job that forks while the
/// member list is being read is still caught; the walk then reaches background
/// jobs an interactive shell moved into groups of their own.
fn signal_session(session_id: i32, signal: i32) {
    // `kill(-1, …)` is every process the user owns, and 0/1 are not sessions
    // this code created. Refuse rather than trust the caller.
    if session_id <= 1 {
        return;
    }
    // SAFETY: plain signal-sending syscalls on ids; no memory is involved.
    unsafe {
        libc::kill(-session_id, signal);
    }
    for pid in session_members(session_id) {
        // SAFETY: as above.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// Live processes belonging to session `session_id`.
///
/// Linux only, via `/proc`; elsewhere the process-group signal above is all
/// there is. A session id cannot be reused as a pid while any process is still
/// in the session, so a match here is never somebody else's process. Zombies are
/// left out: they are already dead, and only waiting to be collected.
fn session_members(session_id: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let (state, session) = parse_stat(&stat)?;
            (session == session_id && state != 'Z').then_some(pid)
        })
        .collect()
}

/// The state letter and session id in a `/proc/<pid>/stat` line.
///
/// The command name is in parentheses and may itself contain spaces and
/// parentheses, so fields are counted from the *last* `)`: after it come
/// state, ppid, pgrp, session.
fn parse_stat(stat: &str) -> Option<(char, i32)> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    let mut fields = after_name.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let session = fields.nth(2)?.parse().ok()?;
    Some((state, session))
}

/// The variables a phone-driven shell may inherit, from `vars`.
fn allowed_environment(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| ALLOWED_ENVIRONMENT.contains(&name) || name.starts_with("LC_"))
        })
        .collect()
}

/// `command` with DeskLink's own environment replaced by the allow-list.
///
/// `CommandBuilder` starts from a copy of this process's environment, which
/// includes whatever DeskLink was launched with. The shell is started in
/// `$HOME` (falling back to the passwd entry, which `portable-pty` also
/// consults) exactly as a terminal emulator would, and argv is untouched: it
/// is never a shell string.
fn restricted(mut command: CommandBuilder) -> CommandBuilder {
    command.env_clear();
    let mut have_home = false;
    let mut have_path = false;
    for (name, value) in allowed_environment(std::env::vars_os()) {
        have_home |= name == "HOME";
        have_path |= name == "PATH";
        command.env(name, value);
    }
    if !have_home {
        if let Some(home) = dirs::home_dir() {
            command.env("HOME", home);
        }
    }
    if !have_path {
        command.env("PATH", FALLBACK_PATH);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command
}

fn clamp_size(rows: u16, cols: u16) -> (u16, u16) {
    (
        rows.clamp(MIN_DIMENSION, MAX_DIMENSION),
        cols.clamp(MIN_DIMENSION, MAX_DIMENSION),
    )
}

/// A short human label for the session list.
fn describe(command: &CommandBuilder) -> String {
    if command.is_default_prog() {
        return "Shell".to_string();
    }
    command
        .get_argv()
        .first()
        .map(|arg| {
            arg.to_string_lossy()
                .chars()
                .take(MAX_TITLE_CHARS)
                .collect()
        })
        .unwrap_or_else(|| "Terminal".to_string())
}

/// A process's name: `/proc/<pid>/comm`, else its argv[0]'s basename.
pub(crate) fn process_name(pid: i32) -> Option<String> {
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        let comm = comm.trim();
        if !comm.is_empty() {
            return Some(comm.to_string());
        }
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = cmdline.split(|byte| *byte == 0).next()?;
    let argv0 = String::from_utf8_lossy(argv0);
    let name = argv0.rsplit('/').next()?.trim_start_matches('-');
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for the reader thread to observe at least `wanted` bytes.
    ///
    /// The reader is a real thread reading a real PTY, so there is no virtual
    /// clock to step here the way `test_link` does — poll with a ceiling rather
    /// than sleeping a fixed amount and hoping.
    fn wait_for_output(session: &PtySession, wanted: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if session.head() >= wanted {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn wait_for_contents(session: &PtySession, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(output) = session.output.lock() {
                if output.parser.screen().contents().contains(needle) {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn echo_session(text: &str) -> PtySession {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", &format!("printf '{text}'")]);
        PtySession::spawn(Some(command), 24, 80).expect("spawn echo session")
    }

    #[test]
    fn a_spawned_program_reaches_both_the_ring_and_the_screen() {
        let session = echo_session("desklink-terminal");
        assert!(wait_for_output(&session, 1), "no output was read");
        assert!(
            wait_for_contents(&session, "desklink-terminal"),
            "screen model never saw the output"
        );
        match session.resync(Some(0), usize::MAX).unwrap() {
            Resync::Bytes { from, data } => {
                assert_eq!(from, 0);
                assert!(String::from_utf8_lossy(&data).contains("desklink-terminal"));
            }
            other => panic!("expected exact replay, got {other:?}"),
        }
    }

    /// The reattach contract: an offset the ring still holds replays exactly,
    /// and anything older falls back to a snapshot rather than a corrupt
    /// stream that starts mid-escape-sequence.
    #[test]
    fn an_unknown_offset_falls_back_to_a_snapshot() {
        let session = echo_session("hello");
        assert!(wait_for_output(&session, 1));
        let head = session.head();
        match session.resync(Some(head + 9_999), usize::MAX).unwrap() {
            Resync::Snapshot { at, data } => {
                assert_eq!(at, head);
                assert!(data.starts_with(b"\x1bc"), "snapshot must reset first");
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    /// A first-time attach has no offset at all and must still get a paintable
    /// screen rather than an empty stream.
    #[test]
    fn a_first_attach_gets_a_snapshot() {
        let session = echo_session("hi");
        assert!(wait_for_output(&session, 1));
        assert!(matches!(
            session.resync(None, usize::MAX).unwrap(),
            Resync::Snapshot { .. }
        ));
    }

    #[test]
    fn resize_moves_the_pty_and_the_screen_model_together() {
        let session = PtySession::spawn(None, 24, 80).expect("spawn shell");
        session.resize(40, 100).expect("resize");
        assert_eq!(session.size(), (40, 100));
        let output = session.output.lock().unwrap();
        assert_eq!(output.parser.screen().size(), (40, 100));
    }

    /// Dimensions arrive from the network. Zero would be rejected by the
    /// kernel and an absurd value would make the screen model allocate without
    /// bound, so both ends are clamped before they reach either.
    #[test]
    fn hostile_dimensions_are_clamped() {
        assert_eq!(clamp_size(0, 0), (MIN_DIMENSION, MIN_DIMENSION));
        assert_eq!(
            clamp_size(u16::MAX, u16::MAX),
            (MAX_DIMENSION, MAX_DIMENSION)
        );
        let session = PtySession::spawn(None, 0, 0).expect("spawn shell");
        assert_eq!(session.size(), (MIN_DIMENSION, MIN_DIMENSION));
    }

    #[test]
    fn input_reaches_the_program() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "read line; printf 'got:%s' \"$line\""]);
        let session = PtySession::spawn(Some(command), 24, 80).expect("spawn reader");
        session.write_input(b"ping\n").expect("write input");
        assert!(
            wait_for_contents(&session, "got:ping"),
            "the program never saw the input"
        );
    }

    #[test]
    fn a_finished_program_reports_its_exit_status() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "exit 3"]);
        let session = PtySession::spawn(Some(command), 24, 80).expect("spawn exiting program");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && session.exit_status().is_none() {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.exit_status(), Some(3));
    }

    /// A program that is not reading stdin fills the PTY's tiny input buffer
    /// after a few KiB. The write used to block in `write_all` on the caller's
    /// thread — the peer worker, holding the bridge and registry locks — so a
    /// large paste stalled SDP, ICE, files and the handover for the whole peer.
    #[test]
    fn writing_to_a_program_that_is_not_reading_never_blocks_the_caller() {
        // Raw mode: in canonical mode the line discipline quietly discards
        // overflow instead of pushing back on the writer.
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "stty raw -echo; echo ready; sleep 30"]);
        let session = PtySession::spawn(Some(command), 24, 80).expect("spawn sleeper");
        assert!(wait_for_contents(&session, "ready"), "stty never ran");

        let (done, finished) = std::sync::mpsc::channel();
        let writer = thread::spawn(move || {
            let chunk = vec![b'x'; 16 * 1024];
            let mut refused = 0;
            for _ in 0..512 {
                if session.write_input(&chunk).is_err() {
                    refused += 1;
                }
            }
            let _ = done.send(refused);
            session.close();
        });
        let refused = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("write_input blocked on a program that is not reading");
        assert!(refused > 0, "8 MiB of input must overflow a bounded queue");
        writer.join().unwrap();
    }

    /// Everything DeskLink itself was started with — TURN credentials, the
    /// DESKLINK_* knobs — must not be readable by a phone-driven shell.
    #[test]
    fn the_child_does_not_inherit_desklink_environment() {
        std::env::set_var("DESKLINK_TURN_SERVERS_TEST_LEAK", "turn:secret@example");
        std::env::set_var("SOME_OTHER_PARENT_SECRET", "hunter2");
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "printf 'leak=[%s][%s] term=[%s] color=[%s] pwd=[%s] home=[%s]' \
             \"${DESKLINK_TURN_SERVERS_TEST_LEAK-unset}\" \"${SOME_OTHER_PARENT_SECRET-unset}\" \
             \"$TERM\" \"$COLORTERM\" \"$(pwd)\" \"$HOME\"; sleep 5",
        ]);
        let session = PtySession::spawn(Some(command), 24, 200).expect("spawn env probe");
        assert!(wait_for_contents(&session, "home=["), "no output");
        let screen = session.output.lock().unwrap().parser.screen().contents();
        assert!(screen.contains("leak=[unset][unset]"), "leaked: {screen}");
        assert!(screen.contains("term=[xterm-256color]"), "{screen}");
        assert!(screen.contains("color=[truecolor]"), "{screen}");
        let home = std::env::var("HOME").unwrap();
        assert!(screen.contains(&format!("pwd=[{home}]")), "{screen}");
        session.close();
    }

    #[test]
    fn only_allow_listed_variables_survive() {
        let vars = [
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("LANG", "en_GB.UTF-8"),
            ("LC_ALL", "C"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("DESKLINK_STUN_SERVERS", "stun:x"),
            ("DESKLINK_TURN_SERVERS", "turn:x"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ]
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let kept: Vec<String> = allowed_environment(vars)
            .into_iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept, ["HOME", "PATH", "LANG", "LC_ALL", "WAYLAND_DISPLAY"]);
    }

    /// `close` used to kill only the direct child. A background job in its own
    /// process group — what an interactive shell makes of `cmd &` — survived
    /// the session that started it.
    #[test]
    fn closing_a_session_kills_its_background_jobs() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "set -m; sleep 300 & echo job=$!; wait"]);
        let session = PtySession::spawn(Some(command), 24, 80).expect("spawn job runner");
        assert!(wait_for_contents(&session, "job="), "job never started");
        let screen = session.output.lock().unwrap().parser.screen().contents();
        let pid: i32 = screen
            .split("job=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|pid| pid.parse().ok())
            .expect("job pid");
        assert!(process_alive(pid), "the job should be running");

        session.close();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && process_alive(pid) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!process_alive(pid), "background job outlived its session");
    }

    /// A program that ignores `SIGHUP` must still die: the hang-up is a courtesy,
    /// the kill after it is the guarantee.
    #[test]
    fn a_program_that_ignores_hangup_is_killed_anyway() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "trap '' HUP; set -m; sleep 300 & echo job=$!; while :; do sleep 1; done",
        ]);
        let session = PtySession::spawn(Some(command), 24, 80).expect("spawn stubborn shell");
        assert!(wait_for_contents(&session, "job="), "job never started");
        let screen = session.output.lock().unwrap().parser.screen().contents();
        let pid: i32 = screen
            .split("job=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|pid| pid.parse().ok())
            .expect("job pid");

        session.close();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && (process_alive(pid) || session.exit_status().is_none()) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!process_alive(pid), "the job survived SIGKILL escalation");
        assert!(session.exit_status().is_some(), "the shell survived");
    }

    #[test]
    fn stat_lines_are_parsed_from_the_last_parenthesis() {
        assert_eq!(parse_stat("42 (bash) S 1 42 42 34816 42"), Some(('S', 42)));
        assert_eq!(parse_stat("7 (a) b (c)) Z 1 9 8 0 0"), Some(('Z', 8)));
        assert_eq!(parse_stat("garbage"), None);
    }

    fn process_alive(pid: i32) -> bool {
        // A killed-but-unreaped process is a zombie: gone as far as we care.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| !stat.contains(") Z "))
            .unwrap_or(false)
    }

    #[test]
    fn a_session_titles_are_bounded() {
        let long = "x".repeat(4000);
        assert!(describe(&CommandBuilder::new(long)).chars().count() <= MAX_TITLE_CHARS);
    }

    #[test]
    fn the_default_session_is_labelled_as_a_shell() {
        assert_eq!(describe(&CommandBuilder::new_default_prog()), "Shell");
        assert_eq!(describe(&CommandBuilder::new("tmux")), "tmux");
    }
}
