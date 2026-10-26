use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::state::push_error;
use super::DaemonWorker;
use crate::device_links::features::is_webrtc_feature;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_CLIPBOARD};
use crate::device_links::pairing::PairState;
use crate::device_links::transport::{should_surface_send_error, FeatureTransport};

static LAST_CLIPBOARD: std::sync::OnceLock<Mutex<String>> = std::sync::OnceLock::new();
static LAST_SENT_BY_DEVICE: std::sync::OnceLock<Mutex<std::collections::HashMap<String, String>>> =
    std::sync::OnceLock::new();

pub(super) fn get_last_clipboard() -> &'static Mutex<String> {
    LAST_CLIPBOARD.get_or_init(|| Mutex::new(String::new()))
}

/// How many clipboard items the Clipboard page lists. Memory only: the
/// history is never written to disk.
const HISTORY_LEN: usize = 20;

/// The MIME type password managers (KeePassXC, KDE's) add to a copied
/// secret. An item carrying it is neither sent nor listed.
const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// One shared clipboard item, for the Clipboard page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardItem {
    pub text: String,
    /// The phone it came from; `None` when it was copied here.
    pub from: Option<String>,
    /// Unix seconds.
    pub at: u64,
}

static HISTORY: Mutex<std::collections::VecDeque<ClipboardItem>> =
    Mutex::new(std::collections::VecDeque::new());

/// The shared items, newest first.
pub fn history() -> Vec<ClipboardItem> {
    HISTORY
        .lock()
        .map(|history| history.iter().cloned().collect())
        .unwrap_or_default()
}

/// Debug builds only: lists an item, for screenshots without a phone.
#[cfg(debug_assertions)]
pub fn debug_remember(text: &str, from: Option<&str>) {
    remember(text, from);
}

/// Debug builds only: a phone's copied image arriving, for isolated tests.
#[cfg(debug_assertions)]
pub fn debug_receive_image(path: &str) {
    if let Ok(bytes) = std::fs::read(path) {
        image_from_phone(&bytes, false);
    }
}

/// Debug builds only: a phone's clipboard arriving, for isolated tests.
#[cfg(debug_assertions)]
pub fn debug_receive(text: &str) {
    let outcome = clipboard_from_phone(text, "Test phone", None);
    debug_log!("[Clipboard] test copy: {outcome}");
}

fn remember(text: &str, from: Option<&str>) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if let Ok(mut history) = HISTORY.lock() {
        push_history(
            &mut history,
            ClipboardItem {
                text: text.to_string(),
                from: from.map(str::to_string),
                at,
            },
        );
    }
}

/// Newest first, capped, and an item copied again moves to the top instead
/// of being listed twice.
fn push_history(history: &mut std::collections::VecDeque<ClipboardItem>, item: ClipboardItem) {
    if item.text.trim().is_empty() {
        return;
    }
    history.retain(|known| known.text != item.text);
    history.push_front(item);
    history.truncate(HISTORY_LEN);
}

fn is_password_hint(targets: &[String]) -> bool {
    targets.iter().any(|target| target == PASSWORD_HINT)
}

/// Whether password-manager hints can be read here: 0 not yet known, 1 yes,
/// 2 no (no X display to ask, so no XWayland either).
static DETECTION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Whether this session can tell a copied password from other text; `None`
/// until it has been tried. Without it, the UI must not promise that
/// passwords stay on this computer.
pub fn password_detection() -> Option<bool> {
    match DETECTION.load(Ordering::Acquire) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

fn record_detection(available: bool) {
    DETECTION.store(if available { 1 } else { 2 }, Ordering::Release);
}

/// Whether the clipboard's owner marked its content as a password. Read
/// from the X11 CLIPBOARD selection's TARGETS, the same selection `arboard`
/// reads (GNOME mirrors Wayland's clipboard into it). An owner that does not
/// answer counts as a secret (see [`withhold`]); no display at all is recorded as detection
/// being unavailable (see [`password_detection`]). Runs on the clipboard
/// thread: the reply wait is bounded at 300 ms, never on the UI's thread.
pub(crate) fn clipboard_is_password() -> bool {
    clipboard_verdict().unwrap_or(true)
}

/// Whether the clipboard holds a password: `Some(answer)` when it could be
/// decided, `None` when the owner did not answer while the question can be
/// asked (no display to ask counts as decided: "not a password").
fn clipboard_verdict() -> Option<bool> {
    let targets = clipboard_targets();
    verdict_of(targets.as_deref(), password_detection())
}

/// [`clipboard_verdict`], asked once more after a second: a busy password
/// manager answers late, and one slow answer must not decide a copy for good.
fn clipboard_verdict_retrying() -> Option<bool> {
    clipboard_verdict().or_else(|| {
        thread::sleep(Duration::from_secs(1));
        clipboard_verdict()
    })
}

/// A copy that could not be checked is neither shared nor remembered as
/// checked (the next look asks again); this is the quiet note for the user.
static UNCHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn clipboard_unchecked() -> bool {
    UNCHECKED.load(Ordering::Relaxed)
}

/// What the targets say: `Some(true)` a password, `Some(false)` not, `None`
/// not decided (the owner did not answer while the question can be asked).
/// No display at all to ask is decided: not a password (the UI says
/// passwords cannot be recognised in this session).
fn verdict_of(targets: Option<&[String]>, detection: Option<bool>) -> Option<bool> {
    match targets {
        Some(targets) => Some(is_password_hint(targets)),
        None if detection == Some(false) => Some(false),
        None => None,
    }
}

fn clipboard_targets() -> Option<Vec<String>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, WindowClass};
    use x11rb::protocol::Event;

    let connected = x11rb::connect(None).ok();
    record_detection(connected.is_some());
    let (conn, screen) = connected?;
    let root = conn.setup().roots.get(screen)?.root;
    let intern = |name: &[u8]| Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom);
    let clipboard = intern(b"CLIPBOARD")?;
    let targets = intern(b"TARGETS")?;
    let property = intern(b"DESKLINK_TARGETS")?;
    let window = conn.generate_id().ok()?;
    conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .ok()?;
    conn.convert_selection(window, clipboard, targets, property, x11rb::CURRENT_TIME)
        .ok()?;
    conn.flush().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        match conn.poll_for_event().ok()? {
            Some(Event::SelectionNotify(notify)) if notify.requestor == window => {
                if notify.property == u32::from(AtomEnum::NONE) {
                    return None;
                }
                let reply = conn
                    .get_property(true, window, property, AtomEnum::ATOM, 0, 1024)
                    .ok()?
                    .reply()
                    .ok()?;
                let names = reply
                    .value32()?
                    .filter_map(|atom| conn.get_atom_name(atom).ok()?.reply().ok())
                    .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
                    .collect();
                return Some(names);
            }
            Some(_) => {}
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
    None
}

/// When a phone's copy last arrived, and when the desk's own clipboard last
/// changed (Unix milliseconds; 0 never).
static REMOTE_AT: AtomicU64 = AtomicU64::new(0);
static LOCAL_CHANGED_AT: AtomicU64 = AtomicU64::new(0);

/// How long a phone's copy may take to land on this clipboard. Text first
/// seen here before that is older than the phone's copy and is never sent
/// back over it.
const SETTLE_MS: u64 = 1500;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whether desk text first seen at `seen_ms` was replaced by a phone's copy
/// received at `remote_ms`: it is then not a new copy to share (the phone's
/// copy is the newer one, even while it is still landing or if it could not
/// be placed).
fn superseded(seen_ms: u64, remote_ms: u64) -> bool {
    remote_ms != 0 && seen_ms < remote_ms.saturating_add(SETTLE_MS)
}

/// `clipboard.connect` (sent on every connection) is the phone's clipboard
/// as of `timestamp`; it replaces the desk's only when newer than the desk's
/// own last change. An unknown time (absent or 0) never does.
fn connect_is_newer(timestamp: Option<u64>, local_changed_ms: u64) -> bool {
    timestamp.is_some_and(|at| at > 0 && at > local_changed_ms)
}

/// What a phone's clipboard packet did here, for the debug log.
pub(super) fn clipboard_from_phone(
    content: &str,
    device_name: &str,
    connect_timestamp: Option<Option<u64>>,
) -> &'static str {
    if !crate::device_links::config::app_settings().clipboard_sync {
        return "ignored: clipboard sync is off";
    }
    match connect_timestamp {
        // A copy made now is always news, even when it is the text the
        // phone sent last time and this computer has copied something else
        // since (it used to be dropped as "unchanged": compared with the
        // last text a *phone* placed, not with what the clipboard holds).
        // Only a duplicate delivery of the copy just placed is skipped.
        None => {
            if just_placed(content, now_ms()) {
                return "unchanged: just placed";
            }
        }
        // The phone's clipboard as of connecting: it replaces this one only
        // when it differs and is newer than this computer's last change.
        Some(timestamp) => {
            if current_text().is_some_and(|current| current == content) {
                return "unchanged";
            }
            if !connect_is_newer(timestamp, LOCAL_CHANGED_AT.load(Ordering::Acquire)) {
                return "ignored: older than this computer's clipboard";
            }
        }
    }
    remember(content, Some(device_name));
    set_clipboard_text_from_remote(content);
    // The Clipboard page shows it now, not at its next one-second look.
    super::state::nudge_tickers();
    "placed on the clipboard"
}

/// The text the clipboard held when it was last looked at or set (what the
/// poll saw, or what a phone placed).
static CURRENT: Mutex<Option<String>> = Mutex::new(None);

fn current_text() -> Option<String> {
    CURRENT.lock().ok().and_then(|current| current.clone())
}

fn note_current(text: &str) {
    if let Ok(mut current) = CURRENT.lock() {
        *current = Some(text.to_string());
    }
}

/// The text last placed from a phone, and when.
static PLACED: Mutex<(String, u64)> = Mutex::new((String::new(), 0));
const DUPLICATE_WITHIN_MS: u64 = 2000;

/// Whether `content` is what was placed from a phone moments ago.
fn just_placed(content: &str, now: u64) -> bool {
    PLACED.lock().is_ok_and(|placed| {
        placed.0 == content && now.saturating_sub(placed.1) < DUPLICATE_WITHIN_MS
    })
}

/// The X11 clipboard context, kept for the life of the process: `arboard`
/// serves our selection from it, so it must outlive every paste. On GNOME
/// and KDE Wayland the compositor mirrors this X11 selection (through
/// XWayland) to every Wayland application.
static X11_CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn set_x11(text: &str, still_latest: &dyn Fn() -> bool) -> bool {
    let Ok(mut held) = X11_CLIPBOARD.lock() else {
        return false;
    };
    // Checked under the lock that serialises every set: a slower thread
    // holding an older copy must not land after a newer one.
    if !still_latest() {
        return true;
    }
    // A second try with a fresh context: the first may be dead (XWayland
    // restarted), and a dead one would refuse every later copy.
    for _ in 0..2 {
        if held.is_none() {
            *held = arboard::Clipboard::new().ok();
        }
        let Some(clipboard) = held.as_mut() else {
            return false;
        };
        if clipboard.set_text(text.to_string()).is_ok() {
            return true;
        }
        *held = None;
    }
    false
}

/// Forgets what was sent to `device_id` (it is no longer paired).
pub(crate) fn forget_sent_to(device_id: &str) {
    if let Some(sent) = LAST_SENT_BY_DEVICE.get() {
        if let Ok(mut sent) = sent.lock() {
            sent.remove(device_id);
        }
    }
}

/// Forgets what was sent to each phone: the clipboard moved on (a phone's
/// copy arrived), so the same text copied again is news again.
pub(crate) fn forget_sent_texts() {
    if let Some(sent) = LAST_SENT_BY_DEVICE.get() {
        if let Ok(mut sent) = sent.lock() {
            sent.clear();
        }
    }
}

/// Whether the text last examined has to be examined again: it is what the
/// clipboard held when it was last looked at, and the clipboard has since
/// held something else (so a copy of it now is a new copy, not the old
/// one).
fn forget_examined(examined: &mut Option<(String, bool, u64)>, current: &str) {
    if examined.as_ref().is_some_and(|(text, ..)| text != current) {
        *examined = None;
    }
}

/// Publishes clipboard text received from a paired device.
///
/// Through X11 first. A Wayland client cannot do this: Mutter (and KWin)
/// accept a Wayland clipboard only from the focused window, and the phone's
/// copy arrives while DeskLink is hidden or in the background — GTK's
/// `Gdk.Clipboard` then claims it locally and nobody else ever sees it.
/// Verified on GNOME Shell 50: X11 text from an unfocused client pastes in
/// Wayland applications; GTK text from an unfocused (or windowless) client
/// does not. With no X display at all, GTK is the last resort: it works
/// while a DeskLink window has focus.
pub(crate) fn set_clipboard_text_from_remote(content: &str) {
    REMOTE_AT.store(now_ms(), Ordering::Release);
    LOCAL_CHANGED_AT.store(now_ms(), Ordering::Release);
    if let Ok(mut placed) = PLACED.lock() {
        *placed = (content.to_string(), now_ms());
    }
    note_current(content);
    // What this desk sent before is no longer what the phone holds: copying
    // it again must send it again.
    forget_sent_texts();
    if let Ok(mut last) = get_last_clipboard().lock() {
        *last = content.to_string();
    }
    let content = content.to_string();
    // Off the packet thread: the first X11 connection can start XWayland.
    thread::spawn(move || {
        // A newer copy may have arrived meanwhile: only the latest is set.
        let latest = || {
            get_last_clipboard()
                .lock()
                .is_ok_and(|last| *last == content)
        };
        if !latest() || set_x11(&content, &latest) {
            return;
        }
        debug_log!("[Clipboard] No X11 clipboard here; placing it through GTK instead");
        gtk::glib::MainContext::default().invoke(move || {
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::prelude::DisplayExt::clipboard(&display).set_text(&content);
            }
        });
    });
}

// ── Images (contract C3) ────────────────────────────────────────────────

/// PNG files start with these 8 bytes.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

pub(crate) fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&PNG_SIGNATURE)
}

/// JPEG files start with an SOI marker and another marker (a phone's
/// photo or scan, F3).
pub(crate) fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[..3] == [0xff, 0xd8, 0xff]
}

/// The longest side of an image sent to a phone (as the phone does).
const MAX_IMAGE_SIDE: i32 = 4096;

/// Bumped each time the X11 CLIPBOARD changes owner (XFixes): an image is
/// read only after a change, never every second.
static OWNER_GEN: AtomicU64 = AtomicU64::new(0);
/// The owner watch is running, so "nothing changed" can be believed.
static OWNER_WATCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Watches CLIPBOARD owner changes on its own X11 connection. Without an X
/// display nothing is watched and images are not sent from here.
/// Marks the owner watch as running for as long as it lives.
struct Watching;

impl Watching {
    fn begin() -> Self {
        OWNER_WATCHED.store(true, Ordering::Release);
        Self
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        OWNER_WATCHED.store(false, Ordering::Release);
    }
}

/// Whether the poll reads the clipboard now: always while the owner is not
/// watched or a phone is connected; otherwise only after the owner changed.
fn should_read(watched: bool, generation: u64, read: u64, nobody: bool) -> bool {
    !(watched && nobody && generation == read)
}

fn watch_owner_changes() {
    let _ = thread::Builder::new()
        .name("desklink-clip-owner".into())
        .spawn(|| {
            use x11rb::connection::Connection;
            use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
            use x11rb::protocol::xproto::ConnectionExt as _;
            use x11rb::protocol::Event;
            let Ok((conn, screen)) = x11rb::connect(None) else {
                return;
            };
            let Some(root) = conn.setup().roots.get(screen).map(|screen| screen.root) else {
                return;
            };
            let watched = (|| {
                let clipboard = conn
                    .intern_atom(false, b"CLIPBOARD")
                    .ok()?
                    .reply()
                    .ok()?
                    .atom;
                conn.xfixes_query_version(5, 0).ok()?.reply().ok()?;
                conn.xfixes_select_selection_input(
                    root,
                    clipboard,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )
                .ok()?;
                conn.flush().ok()
            })();
            if watched.is_none() {
                return;
            }
            // However the watch ends (a lost X connection, a panic), the poll
            // must not go on believing "nothing changed".
            let _watching = Watching::begin();
            while let Ok(event) = conn.wait_for_event() {
                if let Event::XfixesSelectionNotify(_) = event {
                    OWNER_GEN.fetch_add(1, Ordering::AcqRel);
                }
            }
        });
}

/// RGBA pixels (tightly packed) from a PNG or JPEG, for `arboard`.
fn image_to_rgba(image: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let format = if is_png(image) { "png" } else { "jpeg" };
    // A picture claiming more than a screenful is refused from its header.
    let pixbuf = crate::image_limits::decode(
        image,
        Some(format),
        crate::image_limits::MAX_CLIPBOARD_PIXELS,
    )?
    .add_alpha(false, 0, 0, 0)
    .ok()?;
    let (width, height) = (pixbuf.width() as usize, pixbuf.height() as usize);
    let stride = pixbuf.rowstride() as usize;
    let bytes = pixbuf.read_pixel_bytes();
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        rgba.extend_from_slice(bytes.get(row * stride..row * stride + width * 4)?);
    }
    Some((width, height, rgba))
}

/// A PNG from RGBA pixels, its long side brought within [`MAX_IMAGE_SIDE`].
fn rgba_to_png(width: usize, height: usize, rgba: Vec<u8>) -> Option<Vec<u8>> {
    use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
    let (w, h) = (i32::try_from(width).ok()?, i32::try_from(height).ok()?);
    let mut pixbuf = Pixbuf::from_bytes(
        &gtk::glib::Bytes::from_owned(rgba),
        Colorspace::Rgb,
        true,
        8,
        w,
        h,
        w.checked_mul(4)?,
    );
    let long = w.max(h);
    if long > MAX_IMAGE_SIDE {
        let scale = f64::from(MAX_IMAGE_SIDE) / f64::from(long);
        let (sw, sh) = (
            ((f64::from(w) * scale).round() as i32).max(1),
            ((f64::from(h) * scale).round() as i32).max(1),
        );
        pixbuf = pixbuf.scale_simple(sw, sh, InterpType::Bilinear)?;
    }
    pixbuf.save_to_bufferv("png", &[]).ok()
}

/// A phone's copied image (C3), or a photo or scan asked for from here
/// (`asked`, F3, placed even with clipboard sync off): put on this
/// clipboard — X11 first (Mutter mirrors it to Wayland apps), GTK's texture
/// with no X display — and never saved or sent back. PNG or JPEG (a camera
/// photo as PNG is too large), told apart by their first bytes.
pub(crate) fn image_from_phone(png: &[u8], asked: bool) {
    if !asked && !crate::device_links::config::app_settings().clipboard_sync {
        debug_log!("[Clipboard] Image from the phone ignored: clipboard sync is off");
        return;
    }
    if !is_png(png) && !is_jpeg(png) {
        debug_log!("[Clipboard] Image from the phone ignored: neither PNG nor JPEG");
        return;
    }
    // Tests see it here instead: never the real clipboard of whoever runs them.
    #[cfg(test)]
    if let Ok(mut placed) = TEST_PLACED.lock() {
        placed.push(png.to_vec());
    }
    #[cfg(not(test))]
    place_image(png);
}

/// What tests' clipboard images were placed (instead of the clipboard).
#[cfg(test)]
pub(crate) static TEST_PLACED: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

#[cfg(not(test))]
fn place_image(png: &[u8]) {
    REMOTE_AT.store(now_ms(), Ordering::Release);
    LOCAL_CHANGED_AT.store(now_ms(), Ordering::Release);
    debug_log!("[Clipboard] Image from the phone: {} bytes", png.len());
    let png = png.to_vec();
    thread::spawn(move || {
        let placed = image_to_rgba(&png).is_some_and(|(width, height, rgba)| {
            X11_CLIPBOARD.lock().is_ok_and(|mut held| {
                if held.is_none() {
                    *held = arboard::Clipboard::new().ok();
                }
                held.as_mut().is_some_and(|clipboard| {
                    clipboard
                        .set_image(arboard::ImageData {
                            width,
                            height,
                            bytes: rgba.into(),
                        })
                        .is_ok()
                })
            })
        });
        if placed {
            return;
        }
        gtk::glib::MainContext::default().invoke(move || {
            let texture = gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from_owned(png));
            if let (Some(display), Ok(texture)) = (gtk::gdk::Display::default(), texture) {
                gtk::prelude::DisplayExt::clipboard(&display).set_texture(&texture);
            }
        });
    });
}

/// An image the user copied here, as a PNG file to send to the phones, when
/// it is new and not a password manager's. `None` otherwise.
fn copied_image(clipboard: &mut arboard::Clipboard) -> Option<std::path::PathBuf> {
    let targets = clipboard_targets()?;
    if is_password_hint(&targets) || !targets.iter().any(|target| target.starts_with("image/")) {
        return None;
    }
    let image = clipboard.get_image().ok()?;
    let png = rgba_to_png(image.width, image.height, image.bytes.into_owned())?;
    if png.len() as u64 > crate::device_links::webrtc::transfer_manager::MAX_CLIPBOARD_IMAGE_BYTES {
        return None;
    }
    let dir = gtk::glib::user_cache_dir()
        .join("desklink")
        .join("clipboard")
        .join(uuid::Uuid::new_v4().simple().to_string());
    crate::device_links::private_fs::create_dir_all(&dir).ok()?;
    let path = dir.join("clipboard.png");
    debug_log!("[Clipboard] Image copied here: {} bytes of PNG", png.len());
    crate::device_links::private_fs::write(&path, &png).ok()?;
    Some(path)
}

impl DaemonWorker {
    pub(super) fn start_clipboard_listener(&self) {
        if !is_webrtc_feature(PACKET_TYPE_CLIPBOARD) {
            return;
        }
        let sessions = Arc::clone(&self.sessions);
        let shutdown = Arc::clone(&self.shutdown);
        let errors = Arc::clone(&self.errors);
        thread::spawn(move || {
            // The last local text looked at, whether it was a password (so
            // the selection is asked once per copy, not every second), and
            // when it was first seen.
            let mut examined: Option<(String, bool, u64)> = None;
            // Learn up front whether passwords can be recognised here, so the
            // Clipboard page can say so before anything is copied.
            record_detection(x11rb::connect(None).is_ok());
            watch_owner_changes();
            // The owner change last looked at for an image.
            let mut image_gen = OWNER_GEN.load(Ordering::Acquire);
            // One connection to the clipboard, kept (a new one every second
            // was most of what an idle DeskLink did), and the owner change
            // last read for text.
            let mut handle: Option<arboard::Clipboard> = None;
            let mut text_gen = u64::MAX;
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1000));
                if !crate::device_links::config::app_settings().clipboard_sync {
                    handle = None;
                    continue;
                }
                // Nobody to share with and the owner has not changed: there
                // is nothing new to read (the history still records every
                // copy, since a change always passes here).
                let generation = OWNER_GEN.load(Ordering::Acquire);
                if !should_read(
                    OWNER_WATCHED.load(Ordering::Acquire),
                    generation,
                    text_gen,
                    sessions.current_bindings().is_empty(),
                ) {
                    continue;
                }
                text_gen = generation;
                if handle.is_none() {
                    handle = arboard::Clipboard::new().ok();
                }
                let Some(clipboard) = handle.as_mut() else {
                    continue;
                };

                let current_text = match clipboard.get_text() {
                    Ok(text) => text,
                    Err(arboard::Error::ContentNotAvailable) => String::new(),
                    Err(_) => {
                        handle = None;
                        String::new()
                    }
                };
                note_current(&current_text);
                forget_examined(&mut examined, &current_text);
                if current_text.is_empty() {
                    // No text: perhaps an image, read once per copy.
                    let generation = OWNER_GEN.load(Ordering::Acquire);
                    if generation != image_gen {
                        image_gen = generation;
                        let seen = now_ms();
                        if !superseded(seen, REMOTE_AT.load(Ordering::Acquire)) {
                            if let Some(path) = handle.as_mut().and_then(copied_image) {
                                send_image(&sessions, &path);
                                if let Some(dir) = path.parent() {
                                    let _ = std::fs::remove_dir_all(dir);
                                }
                            }
                        }
                    }
                    continue;
                }

                let Ok(last) = get_last_clipboard().lock() else {
                    continue;
                };
                if *last == current_text {
                    // The clipboard holds what a phone put there: what was
                    // examined before is stale, a re-copy of it must be seen.
                    examined = None;
                    continue;
                }
                drop(last);
                if examined
                    .as_ref()
                    .is_none_or(|(text, ..)| *text != current_text)
                {
                    let Some(secret) = clipboard_verdict_retrying() else {
                        // Not cached: the next poll asks again.
                        UNCHECKED.store(true, Ordering::Relaxed);
                        continue;
                    };
                    UNCHECKED.store(false, Ordering::Relaxed);
                    let seen = now_ms();
                    if !secret {
                        remember(&current_text, None);
                    }
                    if !superseded(seen, REMOTE_AT.load(Ordering::Acquire)) {
                        LOCAL_CHANGED_AT.store(seen, Ordering::Release);
                    }
                    examined = Some((current_text.clone(), secret, seen));
                }
                // A password, or the text a phone's newer copy replaced (still
                // landing, or it could not be placed): never sent.
                if examined.as_ref().is_some_and(|(_, secret, seen)| {
                    *secret || superseded(*seen, REMOTE_AT.load(Ordering::Acquire))
                }) {
                    continue;
                }

                for binding in sessions.current_bindings() {
                    let already_sent = LAST_SENT_BY_DEVICE
                        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
                        .lock()
                        .ok()
                        .and_then(|sent| sent.get(&binding.device_id).cloned())
                        .is_some_and(|sent| sent == current_text);
                    if already_sent {
                        continue;
                    }
                    let paired = sessions
                        .with_session(&binding, |session| {
                            session.pairing.state == PairState::Paired
                        })
                        .unwrap_or(false);
                    if !paired || !sessions.is_current(&binding) {
                        continue;
                    }
                    // Skip a device with no usable transport instead of
                    // retrying it every second. The send would fail on a closed
                    // socket, and because the attempt is only recorded on
                    // success the same text would be retried forever.
                    let has_transport = !binding.link.is_closed()
                        || sessions
                            .feature_transport_snapshot(&binding)
                            .map(|snapshot| snapshot.web_rtc_ready)
                            .unwrap_or(false);
                    if !has_transport {
                        continue;
                    }
                    // Sent only over a ready link, never queued: the copy is
                    // latest-wins, and one queued then dropped was marked sent
                    // and never retried.
                    let result =
                        FeatureTransport::for_binding(&sessions, &binding).and_then(|transport| {
                            if !transport.is_ready() {
                                return Ok(false);
                            }
                            let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
                            packet.set("content", current_text.clone());
                            // Too large for one data-channel message: not
                            // shared, and taken as handled so it is not
                            // retried every second.
                            if !crate::device_links::webrtc::fits_on_wire(&packet) {
                                crate::device_links::limits::log_limited(
                                    "clipboard-size",
                                    "A copied text too large to share was not sent",
                                );
                                return Ok(true);
                            }
                            transport.send(&packet).map(|()| true)
                        });
                    if let Ok(mut sent) = LAST_SENT_BY_DEVICE
                        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
                        .lock()
                    {
                        remember_if_sent(&mut sent, &binding.device_id, &current_text, &result);
                    }
                    match result {
                            Ok(_) => {}
                            Err(error) if should_surface_send_error(&error) => {
                                push_error(
                                    &errors,
                                    format!(
                                        "Could not synchronize clipboard over WebRTC: {error}"
                                    ),
                                );
                            }
                            Err(error) => eprintln!(
                                "[DeskLink] Clipboard synchronization waiting for WebRTC recovery: {error}"
                            ),
                        }
                }
            }
        });
    }
}

/// Sends a copied image to every phone with a ready link (the transfer
/// keeps its own snapshot, so `path` may go right after).
fn send_image(sessions: &Arc<crate::device_links::core::DeviceManager>, path: &std::path::Path) {
    for binding in sessions.current_bindings() {
        let paired = sessions
            .with_session(&binding, |session| {
                session.pairing.state == PairState::Paired
            })
            .unwrap_or(false);
        if !paired || !sessions.is_current(&binding) {
            continue;
        }
        let result = FeatureTransport::for_binding(sessions, &binding).and_then(|transport| {
            if transport.is_ready() {
                transport.send_clipboard_image(path)
            } else {
                Ok(())
            }
        });
        match result {
            Ok(()) => debug_log!("[Clipboard] Image sent to {}", binding.device_id),
            Err(error) => debug_log!(
                "[Clipboard] Image not sent to {}: {error}",
                binding.device_id
            ),
        }
    }
}

/// Records `text` as sent to `device_id` only when it really went.
fn remember_if_sent(
    sent: &mut std::collections::HashMap<String, String>,
    device_id: &str,
    text: &str,
    result: &Result<bool, String>,
) {
    if matches!(result, Ok(true)) {
        sent.insert(device_id.to_string(), text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_owner_watch_sends_the_poll_back_to_reading() {
        {
            let _watching = Watching::begin();
            assert!(OWNER_WATCHED.load(Ordering::Acquire));
            assert!(!should_read(true, 1, 1, true), "trusted while it runs");
        }
        // The watcher ended: nothing is believed any more.
        assert!(!OWNER_WATCHED.load(Ordering::Acquire));
        assert!(should_read(
            OWNER_WATCHED.load(Ordering::Acquire),
            1,
            1,
            true
        ));
        // A panicking watcher clears it too.
        let _ = std::panic::catch_unwind(|| {
            let _watching = Watching::begin();
            panic!("the watch fell over");
        });
        assert!(!OWNER_WATCHED.load(Ordering::Acquire));
    }

    #[test]
    fn an_idle_clipboard_is_not_read_until_it_changes() {
        // Watched, nobody to share with, no change: not read.
        assert!(!should_read(true, 4, 4, true));
        // A change, a phone, or no watch at all: read.
        assert!(should_read(true, 5, 4, true), "the owner changed");
        assert!(should_read(true, 4, 4, false), "a phone is connected");
        assert!(should_read(false, 4, 4, true), "no owner watch to trust");
        assert!(should_read(true, 0, u64::MAX, true), "the first look");
    }

    #[test]
    fn a_copy_counts_as_sent_only_after_a_real_send() {
        let mut sent = std::collections::HashMap::new();
        remember_if_sent(&mut sent, "phone", "a", &Ok(false));
        assert!(sent.is_empty(), "not ready: tried again later");
        remember_if_sent(
            &mut sent,
            "phone",
            "a",
            &Err("Session link is closed".into()),
        );
        assert!(sent.is_empty(), "failed: tried again later");
        remember_if_sent(&mut sent, "phone", "a", &Ok(true));
        assert_eq!(sent.get("phone").map(String::as_str), Some("a"));
    }

    fn item(text: &str) -> ClipboardItem {
        ClipboardItem {
            text: text.into(),
            from: None,
            at: 0,
        }
    }

    #[test]
    fn the_history_keeps_the_newest_twenty_once_each() {
        let mut history = std::collections::VecDeque::new();
        for n in 0..25 {
            push_history(&mut history, item(&n.to_string()));
        }
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.front().unwrap().text, "24");
        assert_eq!(history.back().unwrap().text, "5");

        // Copied again: moved to the top, not listed twice.
        push_history(&mut history, item("10"));
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.front().unwrap().text, "10");
        assert_eq!(history.iter().filter(|known| known.text == "10").count(), 1);

        push_history(&mut history, item("  "));
        assert_eq!(
            history.front().unwrap().text,
            "10",
            "blank text is not listed"
        );
    }

    #[test]
    fn a_phone_copy_is_never_sent_back_over_by_older_desk_text() {
        // Desk text seen before the phone's copy arrived, or while it was
        // landing, is older: not sent (it would overwrite the phone's copy).
        assert!(superseded(1_000, 2_000));
        assert!(superseded(2_500, 2_000), "still landing");
        // Copied here after the phone's copy landed: a new copy to share.
        assert!(!superseded(2_000 + SETTLE_MS, 2_000));
        assert!(!superseded(5, 0), "no phone copy yet");
    }

    #[test]
    fn a_reconnect_replaces_the_desk_clipboard_only_with_a_newer_copy() {
        assert!(connect_is_newer(Some(2_000), 1_000));
        assert!(
            !connect_is_newer(Some(1_000), 2_000),
            "the desk copied since"
        );
        assert!(!connect_is_newer(Some(0), 0), "unknown time");
        assert!(!connect_is_newer(None, 0));
        assert!(connect_is_newer(Some(1), 0), "nothing copied here yet");
    }

    #[test]
    fn images_round_trip_through_png_and_large_ones_are_scaled() {
        let rgba: Vec<u8> = (0..4 * 3 * 2).map(|n| n as u8).collect();
        let png = rgba_to_png(3, 2, rgba.clone()).unwrap();
        assert!(is_png(&png));
        assert_eq!(image_to_rgba(&png), Some((3, 2, rgba)));
        assert!(!is_png(b"GIF89a"));
        let big = rgba_to_png(5000, 10, vec![255; 5000 * 10 * 4]).unwrap();
        let (width, height, _) = image_to_rgba(&big).unwrap();
        assert_eq!((width, height), (4096, 8), "long side capped, shape kept");
    }

    /// A phone's photo arrives as JPEG (F3): recognised and decoded too.
    #[test]
    fn a_jpeg_photo_is_recognised_and_decoded() {
        use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
        let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, 6, 4).unwrap();
        pixbuf.fill(0x3366_99ff);
        let jpeg = pixbuf.save_to_bufferv("jpeg", &[]).unwrap();
        assert!(is_jpeg(&jpeg) && !is_png(&jpeg));
        let (width, height, rgba) = image_to_rgba(&jpeg).unwrap();
        assert_eq!((width, height, rgba.len()), (6, 4, 6 * 4 * 4));
        assert!(!is_jpeg(b"\x89PNG"));
    }

    #[test]
    fn an_unanswered_question_withholds_the_clipboard_but_no_display_does_not() {
        let plain = vec!["text/plain".to_string()];
        let marked = vec![PASSWORD_HINT.to_string()];
        assert_eq!(verdict_of(Some(&plain), Some(true)), Some(false));
        assert_eq!(verdict_of(Some(&marked), Some(true)), Some(true));
        // Not decided: neither shared nor remembered as checked (the loop
        // asks again, after a second, and shows "couldn't check it").
        assert_eq!(verdict_of(None, Some(true)), None, "timeout");
        assert_eq!(verdict_of(None, None), None, "not yet known");
        assert_eq!(
            verdict_of(None, Some(false)),
            Some(false),
            "no display to ask"
        );
    }

    /// The owner's report: copying the same text on the phone a second time
    /// (after the computer's clipboard held something else) was dropped as
    /// "unchanged", because it was compared with the last text a phone placed.
    #[test]
    fn the_same_text_copied_again_on_the_phone_is_placed_again() {
        let text = format!("copy-me-{}", uuid::Uuid::new_v4().simple());
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", None),
            "placed on the clipboard"
        );
        // A duplicate delivery of that very copy is skipped...
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", None),
            "unchanged: just placed"
        );
        // ...but, a few seconds later and after the computer copied
        // something else, the phone copying it again is news.
        note_current("something copied at this computer");
        if let Ok(mut placed) = PLACED.lock() {
            placed.1 = now_ms().saturating_sub(DUPLICATE_WITHIN_MS + 1000);
        }
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", None),
            "placed on the clipboard"
        );
        // The history lists it once, newest first.
        let listed: Vec<_> = history()
            .into_iter()
            .filter(|item| item.text == text)
            .collect();
        assert_eq!(listed.len(), 1);
        assert_eq!(history()[0].text, text);
        assert_eq!(history()[0].from.as_deref(), Some("Pixel"));
    }

    /// `clipboard.connect` replaces the computer's clipboard only when it
    /// differs from what is there and is newer than this computer's change.
    #[test]
    fn a_connect_snapshot_is_placed_only_when_different_and_newer() {
        let text = format!("connect-{}", uuid::Uuid::new_v4().simple());
        note_current(&text);
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", Some(Some(now_ms() + 10_000))),
            "unchanged",
            "already the clipboard"
        );
        note_current("something else");
        LOCAL_CHANGED_AT.store(now_ms(), Ordering::Release);
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", Some(Some(1))),
            "ignored: older than this computer's clipboard"
        );
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", Some(None)),
            "ignored: older than this computer's clipboard",
            "an unknown time never replaces"
        );
        assert_eq!(
            clipboard_from_phone(&text, "Pixel", Some(Some(now_ms() + 60_000))),
            "placed on the clipboard"
        );
    }

    /// Two copies arriving close together: the newest is what stays (the
    /// older setter, if slower, steps aside under the lock).
    #[test]
    fn a_slow_older_set_never_lands_after_a_newer_one() {
        let stale = || false;
        // Not the latest any more: nothing is set, and nothing is retried.
        assert!(set_x11("older", &stale));
    }

    /// Packet arrival to the clipboard being set, over a real X11 display (a
    /// dedicated Xvfb named by `DESKLINK_TEST_DISPLAY`; never the real
    /// session). The test re-runs itself with `DISPLAY` set, because `arboard`
    /// reads it from the environment of the whole process. Prints the
    /// latencies (run with `--nocapture`).
    #[test]
    fn a_phones_copy_reaches_a_real_x11_clipboard_within_a_second() {
        const CHILD: &str = "DESKLINK_CLIPBOARD_LATENCY_CHILD";
        let name = "device_links::daemon::clipboard::tests::a_phones_copy_reaches_a_real_x11_clipboard_within_a_second";
        if std::env::var_os(CHILD).is_none() {
            let Some(display) = std::env::var_os("DESKLINK_TEST_DISPLAY") else {
                eprintln!("SKIPPED {name}: DESKLINK_TEST_DISPLAY names no dedicated display");
                return;
            };
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([name, "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .env("DISPLAY", display)
                .env_remove("WAYLAND_DISPLAY")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&output.stdout);
            eprintln!("{text}");
            assert!(
                output.status.success(),
                "{text}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut reader = arboard::Clipboard::new().expect("an X11 display");
        let mut latencies = Vec::new();
        for round in 0..5 {
            let text = format!("latency-{round}-{}", uuid::Uuid::new_v4().simple());
            // Move the clock past the duplicate window between rounds.
            if let Ok(mut placed) = PLACED.lock() {
                placed.1 = 0;
            }
            let started = std::time::Instant::now();
            assert_eq!(
                clipboard_from_phone(&text, "Pixel", None),
                "placed on the clipboard"
            );
            let in_history = started.elapsed();
            let landed = loop {
                if reader.get_text().is_ok_and(|now| now == text) {
                    break started.elapsed();
                }
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(5),
                    "{text} never landed"
                );
                thread::sleep(Duration::from_millis(1));
            };
            println!(
                "round {round}: history {:.1} ms, clipboard {:.1} ms",
                in_history.as_secs_f64() * 1000.0,
                landed.as_secs_f64() * 1000.0
            );
            latencies.push(landed);
        }
        let worst = latencies.iter().max().copied().unwrap();
        assert!(worst < Duration::from_secs(1), "{worst:?}");
    }

    #[test]
    fn password_manager_items_are_recognised_by_their_hint() {
        let plain = vec!["TARGETS".to_string(), "UTF8_STRING".to_string()];
        assert!(!is_password_hint(&plain));
        let mut secret = plain.clone();
        secret.push(PASSWORD_HINT.to_string());
        assert!(is_password_hint(&secret));
    }

    /// Copy A (sent), a phone's copy B arrives, copy A again: the second A is
    /// a new copy and must be examined and sent again, not skipped for ever
    /// because the first one was.
    #[test]
    fn copying_the_same_text_again_after_a_phones_copy_is_sent_again() {
        let mut examined = Some(("A".to_string(), false, 100u64));
        // The clipboard now holds B (what a phone sent): A is forgotten.
        forget_examined(&mut examined, "B");
        assert!(examined.is_none());
        // The same text still on the clipboard keeps its verdict.
        let mut same = Some(("A".to_string(), false, 100u64));
        forget_examined(&mut same, "A");
        assert!(same.is_some());
        // And what was sent to each phone is cleared by a phone's copy.
        let sent = LAST_SENT_BY_DEVICE.get_or_init(|| Mutex::new(Default::default()));
        sent.lock().unwrap().insert("phone-x".into(), "A".into());
        forget_sent_texts();
        assert!(!sent.lock().unwrap().contains_key("phone-x"));
    }
}
