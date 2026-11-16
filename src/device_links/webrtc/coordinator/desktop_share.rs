//! Sharing this computer's screen with the phone: the per-device portal service
//! registry, approval and failure notices, retiring a replaced share, what the
//! UI shows of a share, and ending one. The thread that runs a share is
//! `share_worker`.
//!
//! Sibling edges: none (the parent's helpers only). Outside: `remote_session`,
//! `remote_desktop_portal`, `terminal_bridge`, `daemon::state`, `device`.

use super::*;

/// Merges runs of pointer motion, keeping everything else in order: the latest
/// absolute position wins, relative moves and scrolls add up. A click or key
/// ends a run, so motion is never reordered across it.
pub(super) fn coalesce_motion(actions: Vec<PortalInputAction>) -> Vec<PortalInputAction> {
    let mut merged: Vec<PortalInputAction> = Vec::with_capacity(actions.len());
    for action in actions {
        match (merged.last_mut(), action) {
            (
                Some(PortalInputAction::AbsoluteMotion(x, y)),
                PortalInputAction::AbsoluteMotion(nx, ny),
            ) => {
                (*x, *y) = (nx, ny);
            }
            (
                Some(PortalInputAction::RelativeMotion(dx, dy)),
                PortalInputAction::RelativeMotion(ndx, ndy),
            ) => {
                (*dx, *dy) = (*dx + ndx, *dy + ndy);
            }
            (Some(PortalInputAction::Scroll(dx, dy)), PortalInputAction::Scroll(ndx, ndy)) => {
                (*dx, *dy) = (*dx + ndx, *dy + ndy);
            }
            (_, action) => merged.push(action),
        }
    }
    merged
}

pub(super) enum DesktopPortalCommand {
    Input(Vec<PortalInputAction>),
    /// Show (and control) the next shared screen.
    SwitchScreen,
    /// Ends the share. With a reason, the phone is told it ended (a
    /// `screen-error`) and its session and input lease end with it, so it
    /// leaves the desk screen instead of waiting on a frozen picture.
    /// Without one (a newer share replaces this one) nothing is sent.
    Stop(Option<String>),
}

pub(super) struct DesktopPortalService {
    /// Distinguishes this share from a later one for the same device, so a worker
    /// that is only now finishing can tell whether the registry entry is still
    /// its own before it removes anything.
    pub(super) id: u64,
    pub(super) sender: mpsc::SyncSender<DesktopPortalCommand>,
    pub(super) can_control: bool,
    /// The phone is a second display (F2), not a mirror.
    pub(super) extended: bool,
    /// Set when the share is released or replaced. A worker still waiting on
    /// the GNOME consent dialog gives up, instead of starting a capture nobody
    /// asked for.
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Disconnects when the worker has finished, its portal closed included.
    pub(super) done: mpsc::Receiver<()>,
}

/// The done-signals of shares that were released, per device: the next share
/// for that phone waits on it (≤ [`RETIRE_WAIT`]) before opening its portal.
/// Otherwise a Release→RequestView pair raced two portal sessions, and the
/// old one closing tore down the new one's PipeWire node.
pub(super) fn retiring() -> &'static Mutex<HashMap<String, mpsc::Receiver<()>>> {
    static RETIRING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::Receiver<()>>>> =
        std::sync::OnceLock::new();
    RETIRING.get_or_init(Default::default)
}

/// How long a new share waits for the one it replaces to finish.
const RETIRE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn retire(device_id: &str, done: mpsc::Receiver<()>) {
    if let Ok(mut retiring) = retiring().lock() {
        retiring.insert(device_id.to_string(), done);
    }
}

/// Waits (bounded) until the share `done` belongs to has finished.
pub(super) fn wait_for_retired(done: Option<mpsc::Receiver<()>>) {
    if let Some(done) = done {
        let _ = done.recv_timeout(RETIRE_WAIT);
    }
}

pub(super) static NEXT_PORTAL_SERVICE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub(super) fn desktop_portal_services() -> &'static Mutex<HashMap<String, DesktopPortalService>> {
    static SERVICES: std::sync::OnceLock<Mutex<HashMap<String, DesktopPortalService>>> =
        std::sync::OnceLock::new();
    SERVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Phones whose screen request is waiting for somebody to answer the portal's
/// dialog at this computer, with the share (`id`) that is waiting. The desk
/// window posts a notification for each, so the person at the laptop knows
/// where to look (the dialog is GNOME's, not DeskLink's).
pub(super) fn portal_approvals() -> &'static Mutex<HashMap<String, (u64, std::time::Instant)>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, (u64, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    WAITING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A portal wait shorter than this says nothing: a remembered grant answers at once and
/// shows no dialog, so no notice (and no log line) is made for it.
pub(super) const APPROVAL_NOTICE_AFTER: std::time::Duration =
    std::time::Duration::from_millis(1500);

/// The phones in `waiting` that have waited at least `after` at `now`.
pub(super) fn waited_long_enough(
    waiting: &HashMap<String, (u64, std::time::Instant)>,
    now: std::time::Instant,
    after: std::time::Duration,
) -> Vec<String> {
    waiting
        .iter()
        .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= after)
        .map(|(device, _)| device.clone())
        .collect()
}

/// The phones (by id) that have waited for approval at this computer long enough to be worth a notice.
pub(crate) fn phones_awaiting_approval() -> Vec<String> {
    portal_approvals()
        .lock()
        .map(|waiting| {
            waited_long_enough(&waiting, std::time::Instant::now(), APPROVAL_NOTICE_AFTER)
        })
        .unwrap_or_default()
}

/// Shares that failed to start here, by phone, with the reason the phone
/// was given: the window says so at this computer too, so neither side is
/// left wondering why nothing appeared.
pub(super) fn share_failures() -> &'static Mutex<Vec<(String, String)>> {
    static FAILED: std::sync::OnceLock<Mutex<Vec<(String, String)>>> = std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

/// Takes the failures not yet shown (phone id, reason).
pub(crate) fn take_share_failures() -> Vec<(String, String)> {
    share_failures()
        .lock()
        .map(|mut failed| std::mem::take(&mut *failed))
        .unwrap_or_default()
}

/// Whether a failed share is worth telling the person at this computer:
/// not when they declined it themselves.
pub(super) fn tell_desk_about_failure(reason: &str) -> bool {
    reason != crate::device_links::webrtc::remote_desktop_portal::DECLINED_HERE
        && reason != crate::device_links::webrtc::remote_desktop_portal::LOCKED_HERE
}

/// Held while a share waits for the portal's answer; gone when it answers
/// (allowed, refused, cancelled or timed out), however the worker ends. Only
/// the share that registered it can clear it, so a replaced share's end does
/// not withdraw its successor's notice.
pub(super) struct AwaitingApproval {
    device_id: String,
    id: u64,
}

impl AwaitingApproval {
    pub(super) fn begin(device_id: &str, id: u64) -> Self {
        if let Ok(mut waiting) = portal_approvals().lock() {
            waiting.insert(device_id.to_string(), (id, std::time::Instant::now()));
        }
        // Once the wait has lasted long enough to be a dialog somebody must answer: the one
        // line (not debug_log!) that says a phone's request got here, and the notice, now
        // rather than at the next tick. A grant answered sooner leaves neither.
        let (device, share) = (device_id.to_string(), id);
        let _ = thread::Builder::new()
            .name("desklink-approval-notice".into())
            .spawn(move || {
                thread::sleep(APPROVAL_NOTICE_AFTER);
                let still_waiting = portal_approvals()
                    .lock()
                    .is_ok_and(|waiting| waiting.get(&device).is_some_and(|(id, _)| *id == share));
                if still_waiting {
                    eprintln!(
                        "[DeskLink] {device} asked to see this screen; waiting for it to be allowed here"
                    );
                    crate::device_links::daemon::state::nudge_tickers();
                }
            });
        Self {
            device_id: device_id.to_string(),
            id,
        }
    }
}

impl Drop for AwaitingApproval {
    fn drop(&mut self) {
        if let Ok(mut waiting) = portal_approvals().lock() {
            if waiting
                .get(&self.device_id)
                .is_some_and(|(id, _)| *id == self.id)
            {
                waiting.remove(&self.device_id);
            }
        }
        // The notice is withdrawn now, not at the next tick.
        crate::device_links::daemon::state::nudge_tickers();
    }
}

/// Removes `device_id`'s registry entry — but only if it is still `id`'s.
///
/// A worker finishing after a newer share has registered used to remove *that*
/// share's entry by device id, which ended the new capture right after the user
/// had approved it.
pub(super) fn remove_service_if_current(device_id: &str, id: u64) -> bool {
    let Ok(mut services) = desktop_portal_services().lock() else {
        return false;
    };
    if services
        .get(device_id)
        .is_some_and(|service| service.id == id)
    {
        services.remove(device_id);
        true
    } else {
        false
    }
}

/// The largest second display accepted, in pixels (a 4K screen and a bit),
/// and the smallest side.
const MAX_EXTEND_PIXELS: i64 = 3840 * 2400;
const MIN_EXTEND_SIDE: i32 = 240;

/// Whether a second display of this size may be made.
pub(super) fn extend_size_ok(width: i32, height: i32) -> bool {
    width >= MIN_EXTEND_SIDE
        && height >= MIN_EXTEND_SIDE
        && i64::from(width) * i64::from(height) <= MAX_EXTEND_PIXELS
}

/// What `device_id` is doing with this computer's screen, for the UI.
pub(crate) fn desktop_session(device_id: &str) -> crate::device_links::device::DesktopSession {
    use crate::device_links::device::DesktopSession;
    let sharing = desktop_portal_services()
        .lock()
        .is_ok_and(|services| services.contains_key(device_id));
    match (sharing, peer_input_active(device_id)) {
        (false, _) => DesktopSession::None,
        (true, false) => DesktopSession::Viewing,
        (true, true) => DesktopSession::Controlling,
    }
}

/// Whether `device_id`'s share of this desk is a second display (F2).
pub(crate) fn desktop_extended(device_id: &str) -> bool {
    desktop_portal_services().lock().is_ok_and(|services| {
        services
            .get(device_id)
            .is_some_and(|service| service.extended)
    })
}

/// "End phone sessions": stops `device_id` viewing or controlling this screen
/// and detaches its terminals. Pairing and consent are untouched; the phone
/// may start again.
pub(crate) fn end_phone_sessions(device_id: &str) {
    let viewing = desktop_portal_services()
        .lock()
        .is_ok_and(|services| services.contains_key(device_id));
    let terminals = crate::device_links::webrtc::terminal_bridge::attached_count(device_id) > 0;
    if viewing || terminals {
        connection_log::record(device_id, ConnectionEvent::SessionsEnded);
    }
    stop_desktop_portal_session(device_id, Some(SESSIONS_ENDED_AT_DESK));
    crate::device_links::webrtc::terminal_bridge::end_device_sessions(device_id);
}

/// What the phone is told when the desk ends its view of the screen.
const SESSIONS_ENDED_AT_DESK: &str = "Sessions were ended at the desk";
pub(super) const SHARING_STOPPED_AT_DESK: &str = "Sharing was stopped at the desk";

/// Stops `device_id` viewing or controlling this screen, leaving its
/// terminals attached: "Stop Sharing" at the desk.
pub(super) fn end_screen_session(device_id: &str) {
    stop_desktop_portal_session(device_id, Some(SHARING_STOPPED_AT_DESK));
}

pub(super) fn stop_desktop_portal_session(device_id: &str, tell: Option<&str>) {
    let service = desktop_portal_services()
        .lock()
        .ok()
        .and_then(|mut services| services.remove(device_id));
    if let Some(DesktopPortalService {
        cancel,
        sender,
        done,
        ..
    }) = service
    {
        connection_log::record(device_id, ConnectionEvent::ScreenSessionEnded);
        // The next share for this phone waits for this one to finish.
        retire(device_id, done);
        // Cancelling too: a worker still waiting on the consent dialog never
        // reads the `Stop` command until that dialog is answered.
        cancel.store(true, std::sync::atomic::Ordering::Release);
        let _ = sender.try_send(DesktopPortalCommand::Stop(tell.map(str::to_string)));
    }
}

/// Commands waiting for one device's share worker, at most. A command is an
/// input burst, a screen switch or a stop: a worker that is this far behind
/// is blocked on the portal.
pub(super) const PORTAL_QUEUE: usize = 256;

/// Whether the phone currently holds a valid input lease for `device_id`.
pub(super) fn peer_input_active(device_id: &str) -> bool {
    remote_session::sessions()
        .lock()
        .ok()
        .and_then(|open| {
            open.get(device_id)
                .map(|session| session.state() == remote_session::SessionState::PeerControlling)
        })
        .unwrap_or(false)
}
