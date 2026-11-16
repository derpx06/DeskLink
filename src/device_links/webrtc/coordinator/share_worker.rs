//! The share worker: one thread per shared screen that starts the portal
//! session (with retries and the extended display), forwards the phone's
//! input, watches the capture's health and cleans up however it ends.
//!
//! Sibling edges: `desktop_share` (the service registry, approvals, failure
//! notices), `phone_remote` (`send_remote_session_message`), and the parent's
//! helpers (`now_millis`). Outside: `remote_desktop_portal`, `screen`,
//! `remote_session`, `text_focus`, `virtual_display`, `config` (the remembered
//! grant), `daemon::state`.

use super::*;

/// Runs when a portal worker ends, **however** it ends: every early return and
/// an unwinding panic included. The scattered per-return cleanup it replaces
/// missed the paths that left the registry entry behind, after which every later
/// share for the device failed as "already active".
struct PortalWorkerCleanup {
    device_id: String,
    id: u64,
    peer: Arc<DesktopWebRtcPeer>,
    /// Dropped last of all: tells a share waiting to replace this one.
    _done: mpsc::Sender<()>,
}

/// Declared right after the portal, so it runs *before* the portal closes:
/// the capture is stopped (pipeline to NULL) while its PipeWire node still
/// exists. The other way round, `Session.Close` removed the node under a
/// PLAYING `pipewiresrc` (`not-negotiated`, `not-linked`).
struct StopCaptureFirst {
    id: u64,
    peer: Arc<DesktopWebRtcPeer>,
}

impl Drop for StopCaptureFirst {
    fn drop(&mut self) {
        self.peer.stop_desktop_screen_capture_for(self.id);
    }
}

impl Drop for PortalWorkerCleanup {
    fn drop(&mut self) {
        remove_service_if_current(&self.device_id, self.id);
        self.peer.stop_desktop_screen_capture_for(self.id);
    }
}

/// A share whose capture produces nothing for this long is rebuilt. The
/// source re-sends its last frame every 200 ms even on a still desktop, so
/// silence means the stream died (PipeWire errors, a failed negotiation) and
/// the phone would otherwise wait for a picture forever.
pub(super) const CAPTURE_STALL: std::time::Duration = std::time::Duration::from_secs(3);
/// Rebuilds in a row without steady video before the share is reported
/// failed to the phone (`screen-stopped`, "not producing any video"): about
/// 15 s, which outlasts a WirePlumber restart (measured: back within ~1 s,
/// and it crashed several times in a minute when bluetoothd did).
pub(super) const CAPTURE_RESTARTS: u32 = 5;

/// Why a share ended when its capture never came back.
const STOPPED_SENDING: &str = "The computer stopped sending its screen. Try again; if it keeps happening, restart DeskLink on the computer.";

/// What the capture watchdog does on one look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureCheck {
    /// Video moved; `reset` clears the rebuild count (it kept flowing long
    /// enough after the last rebuild).
    Flowing { reset: bool },
    /// Quiet, not for long enough to act on.
    Wait,
    /// Quiet because the link is not ready (connecting, a blip, the phone
    /// dozing): `webrtcbin` holds the capture's thread until it is, so the
    /// silence says nothing about the capture. The stall clock stops.
    Hold,
    /// Rebuild the capture on the same portal stream (no dialog, same SSRC).
    Rebuild,
    /// Rebuilt enough times: tell the phone this share is not working.
    GiveUp,
}

/// The watchdog's decision. Pure: `progressed` is whether frames were sent
/// since the last look, `quiet_for` how long since they last were,
/// `since_rebuild` how long since the last rebuild (if any).
pub(super) fn capture_check(
    progressed: bool,
    link_ready: bool,
    quiet_for: std::time::Duration,
    since_rebuild: Option<std::time::Duration>,
    restarts: u32,
) -> CaptureCheck {
    if progressed {
        // Only video that keeps flowing clears the count: a rebuilt chain
        // that let a few packets out and then died reset it every time,
        // and the share rebuilt every 4 s for as long as the phone looked.
        CaptureCheck::Flowing {
            reset: since_rebuild.is_none_or(|since| since >= CAPTURE_HEALTHY_AFTER),
        }
    } else if !link_ready {
        CaptureCheck::Hold
    } else if quiet_for < CAPTURE_STALL {
        CaptureCheck::Wait
    } else if restarts >= CAPTURE_RESTARTS {
        CaptureCheck::GiveUp
    } else {
        CaptureCheck::Rebuild
    }
}
/// How long a rebuilt capture must keep sending before the rebuild count
/// starts over.
const CAPTURE_HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);
/// Scroll steps closer together than this are one gesture.
const SCROLL_FINISH_AFTER: std::time::Duration = std::time::Duration::from_millis(150);

pub(super) fn desktop_capture_needed(direction: remote_session::ScreenDirection) -> bool {
    direction == remote_session::ScreenDirection::DesktopToPhone
}

/// Attempts at starting a capture, and the pause between them.
pub(super) const CAPTURE_START_ATTEMPTS: u32 = 4;
const CAPTURE_START_PAUSE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Starts a capture, trying again a few times. Measured on this laptop: a
/// crashing bluetoothd takes WirePlumber down with it, and a share started
/// while WirePlumber restarts (about a second) found its PipeWire stream
/// unlinked: no frames, or "Failed to sync state with parent". Stops early
/// when the share is cancelled; the last error is returned.
/// `start(attempt)` is called with 1 first; later attempts must use the
/// owner-checked start, so a stale retry never takes a newer share's capture.
pub(super) fn start_capture_retrying(
    cancel: &std::sync::atomic::AtomicBool,
    pause: impl Fn(std::time::Duration),
    mut start: impl FnMut(u32) -> Result<(), String>,
) -> Result<(), String> {
    let cancelled = || cancel.load(std::sync::atomic::Ordering::Acquire);
    let mut attempt = 1;
    loop {
        match start(attempt) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < CAPTURE_START_ATTEMPTS && !cancelled() => {
                eprintln!(
                    "[DeskLink] Screen capture did not start ({error}); trying again ({attempt} of {CAPTURE_START_ATTEMPTS})"
                );
                pause(CAPTURE_START_PAUSE);
                // Released or replaced during the pause: no more attempts.
                if cancelled() {
                    return Err(error);
                }
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// A copy of the portal's PipeWire connection for one capture attempt.
fn dup_pipewire_fd(fd: &std::os::fd::OwnedFd) -> Result<std::os::fd::OwnedFd, String> {
    fd.try_clone()
        .map_err(|error| format!("Could not keep the PipeWire connection: {error}"))
}

/// The phone as a second display (F2): a virtual monitor of `pixels`, its
/// scale set so it is readable, input through EIS. Returns what the share
/// worker needs, with the monitor's logical size (what the phone maps its
/// taps into).
#[allow(clippy::type_complexity)]
fn start_extended_display(
    device_id: &str,
    id: u64,
    cancel: &std::sync::atomic::AtomicBool,
    peer: &Arc<DesktopWebRtcPeer>,
    request: crate::device_links::webrtc::screen::ScreenRequest,
    pixels: (i32, i32),
) -> Result<
    (
        Box<dyn DesktopSession>,
        i32,
        i32,
        bool,
        Option<std::os::fd::OwnedFd>,
    ),
    String,
> {
    use crate::device_links::webrtc::virtual_display::{self, VirtualDisplay};
    // Its own remembered grant: a mirror's would select a monitor.
    let key = format!("{device_id}:extend");
    let stored = crate::device_links::config::remote_desktop_token(&key);
    let (display, source) = match VirtualDisplay::start(cancel, stored.as_deref(), pixels) {
        Err(error) if stored.is_some() => {
            debug_log!(
                "[DeskLink] Remembered extended-display grant failed ({error}); asking again"
            );
            crate::device_links::config::set_remote_desktop_token(&key, None);
            VirtualDisplay::start(cancel, None, pixels)?
        }
        started => started?,
    };
    crate::device_links::config::set_remote_desktop_token(&key, source.token_to_remember());
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err("The desktop share was cancelled".to_string());
    }
    let switch_fd = source
        .fd
        .try_clone()
        .map_err(|error| format!("Could not keep the PipeWire connection: {error}"))?;
    let before = virtual_display::connectors();
    start_capture_retrying(cancel, std::thread::sleep, |attempt| {
        let start = if attempt == 1 {
            DesktopWebRtcPeer::start_desktop_screen_capture
        } else {
            DesktopWebRtcPeer::retry_desktop_screen_capture
        };
        start(
            peer,
            id,
            DesktopSource::PipeWire {
                fd: dup_pipewire_fd(&source.fd)?,
                node_id: source.node_id,
                serial: source.pipewire_serial,
                virtual_size: Some(pixels),
            },
            pixels.0,
            pixels.1,
            request,
        )
    })?;
    // The monitor appears once the capture has negotiated its size.
    let scale = virtual_display::apply_scale(&before, pixels);
    let logical = virtual_display::logical_size(pixels, scale);
    display.settle(logical);
    debug_log!(
        "[DeskLink] Extended display {}x{} px at scale {scale} ({}x{} logical) for {device_id}",
        pixels.0,
        pixels.1,
        logical.0,
        logical.1
    );
    let can_control = display.can_control();
    Ok((
        Box::new(display),
        logical.0,
        logical.1,
        can_control,
        Some(switch_fd),
    ))
}

pub(super) fn start_desktop_portal_session(
    device_id: String,
    session_id: String,
    attempt_id: String,
    wire: WebRtcWireBinding,
    peer: Arc<DesktopWebRtcPeer>,
    request: crate::device_links::webrtc::screen::ScreenRequest,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    let id = NEXT_PORTAL_SERVICE_ID.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(PORTAL_QUEUE);
    let (done_sender, done) = mpsc::channel();
    let previous_done = {
        let mut services = desktop_portal_services()
            .lock()
            .map_err(|_| "Desktop portal service registry lock poisoned".to_string())?;
        // A newer request supersedes an earlier share for this phone — a retry,
        // or an older phone build that never released the previous one. Refusing
        // it as "already active" left every later request denied.
        let previous = services.remove(&device_id).map(|previous| {
            previous.cancel.store(true, Ordering::Release);
            let _ = previous.sender.try_send(DesktopPortalCommand::Stop(None));
            previous.done
        });
        services.insert(
            device_id.clone(),
            DesktopPortalService {
                id,
                sender,
                can_control: false,
                extended: request.extend.is_some(),
                cancel: Arc::clone(&cancel),
                done,
            },
        );
        // Else one released a moment ago may still be closing.
        previous.or_else(|| {
            retiring()
                .lock()
                .ok()
                .and_then(|mut retiring| retiring.remove(&device_id))
        })
    };
    let worker_device_id = device_id.clone();
    let spawned = thread::Builder::new()
        .name(format!("desklink-portal-{device_id}"))
        .spawn(move || {
            run_desktop_portal_worker(
                worker_device_id,
                id,
                session_id,
                attempt_id,
                wire,
                peer,
                cancel,
                receiver,
                (done_sender, previous_done),
                request,
            )
        });
    if let Err(error) = spawned {
        remove_service_if_current(&device_id, id);
        return Err(format!(
            "Could not start the desktop portal worker: {error}"
        ));
    }
    Ok(())
}

/// One desktop-screen share: consent, capture, input dispatch, teardown.
#[allow(clippy::too_many_arguments)]
fn run_desktop_portal_worker(
    device_id: String,
    id: u64,
    session_id: String,
    attempt_id: String,
    wire: WebRtcWireBinding,
    peer: Arc<DesktopWebRtcPeer>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    receiver: mpsc::Receiver<DesktopPortalCommand>,
    (done, previous_done): (mpsc::Sender<()>, Option<mpsc::Receiver<()>>),
    request: crate::device_links::webrtc::screen::ScreenRequest,
) {
    use std::sync::atomic::Ordering;

    // Declared first, so it runs last: after every lock and portal handle below
    // has been released, on every way out of this function.
    let _cleanup = PortalWorkerCleanup {
        device_id: device_id.clone(),
        id,
        peer: Arc::clone(&peer),
        _done: done,
    };
    // The share this one replaces closes its portal first.
    wait_for_retired(previous_done);

    #[allow(clippy::type_complexity)]
    let setup =
        (|| -> Result<(Box<dyn DesktopSession>, i32, i32, bool, Option<std::os::fd::OwnedFd>), String> {
            if crate::device_links::webrtc::x11_desktop::wanted() {
                // No portal, so no dialog here: only a phone approved at this
                // desk (Remote Control → Approve…) may share it.
                if crate::device_links::config::remote_desktop_token(&device_id).as_deref()
                    != Some(crate::device_links::webrtc::x11_desktop::DESK_APPROVAL)
                {
                    return Err("This X11 desktop has no screen-sharing portal to ask permission with. \
                                Approve this phone in DeskLink on the computer first \
                                (device page → Remote Control → Approve…)"
                        .to_string());
                }
                let desk = crate::device_links::webrtc::x11_desktop::X11Desktop::connect()?;
                let screen = desk.active();
                peer.start_desktop_screen_capture(
                    id,
                    DesktopSource::X11,
                    screen.width,
                    screen.height,
                    request,
                )?;
                let can_control = desk.can_control();
                return Ok((Box::new(desk), screen.width, screen.height, can_control, None));
            }
            // From here the portal may put its dialog up at this computer.
            let _awaiting = AwaitingApproval::begin(&device_id, id);
            if let Some(pixels) = request.extend {
                return start_extended_display(&device_id, id, &cancel, &peer, request, pixels);
            }
            let stored_grant = crate::device_links::config::remote_desktop_token(&device_id);
            let (portal, source) =
                match RemoteDesktopPortal::start("", &cancel, stored_grant.as_deref()) {
                    // A remembered grant for screens that changed: forget it
                    // and ask once more without it (GNOME shows its dialog).
                    Err(error)
                        if stored_grant.is_some()
                            && error == crate::device_links::webrtc::remote_desktop_portal::NO_SCREEN_STREAM =>
                    {
                        debug_log!("[DeskLink] Remembered screen grant gave no screen; asking again");
                        crate::device_links::config::set_remote_desktop_token(&device_id, None);
                        RemoteDesktopPortal::start("", &cancel, None)?
                    }
                    started => started?,
                };
            // Tokens are single-use: keep the fresh one (if it carries control).
            crate::device_links::config::set_remote_desktop_token(
                &device_id,
                source.token_to_remember(),
            );
            let can_control = source.granted_device_types & 3 == 3;
            if cancel.load(Ordering::Acquire) {
                // Approved in the instant the share was released or replaced.
                return Err("The desktop share was cancelled".to_string());
            }
            // One PipeWire connection serves every shared screen; a copy is kept
            // for switching between them.
            let switch_fd = source
                .fd
                .try_clone()
                .map_err(|error| format!("Could not keep the PipeWire connection: {error}"))?;
            start_capture_retrying(&cancel, std::thread::sleep, |attempt| {
                let start = if attempt == 1 {
                    DesktopWebRtcPeer::start_desktop_screen_capture
                } else {
                    DesktopWebRtcPeer::retry_desktop_screen_capture
                };
                start(
                    &peer,
                    id,
                    DesktopSource::PipeWire {
                        fd: dup_pipewire_fd(&source.fd)?,
                        node_id: source.node_id,
                        serial: source.pipewire_serial,
                        virtual_size: None,
                    },
                    source.width,
                    source.height,
                    request,
                )
            })?;
            Ok((
                Box::new(portal),
                source.width,
                source.height,
                can_control,
                Some(switch_fd),
            ))
        })();
    let (portal, width, height, can_control, switch_fd) = match setup {
        Ok(ready) => ready,
        Err(error) => {
            // Released or replaced: nobody is waiting for an answer to *this*
            // request, and a denial quoting its id would only confuse the phone.
            if cancel.load(Ordering::Acquire) || !remove_service_if_current(&device_id, id) {
                return;
            }
            eprintln!(
                "[DeskLink] Desktop screen/control portal setup failed for {device_id}: {error}"
            );
            let reason =
                crate::device_links::webrtc::remote_desktop_portal::plain_portal_error_when(
                    &error,
                    crate::desktop_env::locked(),
                );
            if tell_desk_about_failure(&reason) {
                if let Ok(mut failed) = share_failures().lock() {
                    failed.push((device_id.clone(), reason.clone()));
                }
                crate::device_links::daemon::state::nudge_tickers();
            }
            let now = now_millis();
            let denied = remote_session::sessions().lock().ok().and_then(|mut open| {
                open.get_mut(&device_id).map(|session| {
                    let mut denied = remote_session::ControlMessage::new(
                        remote_session::ControlKind::ViewDenied,
                        &attempt_id,
                        &wire,
                        &session_id,
                        session.next_sequence(),
                        now,
                    );
                    denied.reason = Some(reason);
                    let _ = session.accept(&denied, now);
                    denied
                })
            });
            if let Some(denied) = denied {
                let _ = send_remote_session_message(&peer, &wire, &denied);
            }
            return;
        }
    };
    // Dropped before `portal`: see `StopCaptureFirst`.
    let _stop_capture_first = StopCaptureFirst {
        id,
        peer: Arc::clone(&peer),
    };
    debug_log!(
        "[DeskLink] Desktop portal granted screen {width}x{height} (1 of {} shared) and input-device mask {} for {device_id}",
        portal.stream_count(),
        if can_control { 3 } else { 0 }
    );
    {
        // One health line a few seconds in: a share that sends nothing is
        // otherwise invisible from this side.
        let (peer, device_id) = (Arc::clone(&peer), device_id.clone());
        let _ = thread::Builder::new()
            .name("desklink-share-health".into())
            .spawn(move || {
                thread::sleep(std::time::Duration::from_secs(3));
                debug_log!(
                    "[DeskLink] Desktop share to {device_id}: {} RTP packet groups after 3 s",
                    peer.desktop_screen_frames_sent()
                );
            });
    }
    // Record what the portal granted — on this share's entry only.
    match desktop_portal_services().lock() {
        Ok(mut services) => match services
            .get_mut(&device_id)
            .filter(|service| service.id == id)
        {
            Some(service) => service.can_control = can_control,
            None => return, // superseded while consent was pending
        },
        Err(_) => return,
    }

    // Move to `viewing` and learn the sequence for the announcement — but only
    // if this share is still the session's current one.
    let sequence = {
        let Ok(mut open) = remote_session::sessions().lock() else {
            return;
        };
        let Some(session) = open.get_mut(&device_id) else {
            return;
        };
        if session.remote_session_id() != Some(session_id.as_str()) {
            return;
        }
        let mut local_ready = remote_session::ControlMessage::new(
            remote_session::ControlKind::ScreenReady,
            &attempt_id,
            &wire,
            &session_id,
            session.next_sequence(),
            now_millis(),
        );
        local_ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
        if let Err(error) = session.accept(&local_ready, now_millis()) {
            eprintln!("[DeskLink] Could not activate desktop screen session: {error}");
            return;
        }
        local_ready.sequence
    };
    let mut ready = remote_session::ControlMessage::new(
        remote_session::ControlKind::ScreenReady,
        &attempt_id,
        &wire,
        &session_id,
        sequence,
        now_millis(),
    );
    ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
    // The desktop's own coordinate space: the phone maps its taps into this, not
    // into the (downscaled) picture it decodes.
    ready.screen_width = Some(width);
    ready.screen_height = Some(height);
    ready.screen_count = Some(portal.stream_count() as i32);
    if request.extend.is_some() {
        ready.display = Some("extend".to_string());
    }
    if send_remote_session_message(&peer, &wire, &ready).is_err() {
        return;
    }
    // (Re)starts capture of `stream` on the share's PipeWire connection.
    let capture = |stream: crate::device_links::webrtc::remote_desktop_portal::PortalStream| -> Result<(), String> {
        let source = match &switch_fd {
            Some(fd) => DesktopSource::PipeWire {
                fd: fd
                    .try_clone()
                    .map_err(|error| format!("Could not reuse the PipeWire connection: {error}"))?,
                node_id: stream.node_id,
                serial: stream.pipewire_serial,
                virtual_size: request.extend,
            },
            None => DesktopSource::X11,
        };
        // An extended display is captured at its pixel size, not the
        // logical one the phone maps its taps into.
        let (width, height) = request.extend.unwrap_or((stream.width, stream.height));
        peer.start_desktop_screen_capture(id, source, width, height, request)
    };
    // Moves the share to the next screen and tells the phone its new size.
    let switch_screen = || {
        let next = portal.next_stream();
        if let Err(error) = capture(next) {
            eprintln!("[DeskLink] Could not switch screens: {error}");
            return;
        }
        let sequence = remote_session::sessions().lock().ok().and_then(|mut open| {
            open.get_mut(&device_id)
                .map(|session| session.next_sequence())
        });
        if let Some(sequence) = sequence {
            let mut ready = remote_session::ControlMessage::new(
                remote_session::ControlKind::ScreenReady,
                &attempt_id,
                &wire,
                &session_id,
                sequence,
                now_millis(),
            );
            ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
            ready.screen_width = Some(next.width);
            ready.screen_height = Some(next.height);
            ready.screen_count = Some(portal.stream_count() as i32);
            let _ = send_remote_session_message(&peer, &wire, &ready);
        }
        debug_log!(
            "[DeskLink] Desktop share for {device_id} switched to a {}x{} screen",
            next.width,
            next.height
        );
    };

    // While the phone can type here, tell it when a text input takes focus,
    // so its keyboard comes up on its own. Lives as long as this share.
    let text_focus = can_control.then(|| {
        let (peer, wire, attempt_id, session_id, device_id) = (
            Arc::clone(&peer),
            wire.clone(),
            attempt_id.clone(),
            session_id.clone(),
            device_id.clone(),
        );
        crate::device_links::webrtc::text_focus::TextFocusWatcher::start(move |editable| {
            let message = remote_session::sessions().lock().ok().and_then(|mut open| {
                let session = open.get_mut(&device_id)?;
                let mut message = remote_session::ControlMessage::new(
                    remote_session::ControlKind::InputFocus,
                    &attempt_id,
                    &wire,
                    &session_id,
                    session.next_sequence(),
                    now_millis(),
                );
                message.editable = Some(editable);
                Some(message)
            });
            let Some(message) = message else {
                debug_log!(
                    "[DeskLink] Text focus (editable {editable}) not sent to {device_id}: no remote session"
                );
                return;
            };
            match send_remote_session_message(&peer, &wire, &message) {
                Ok(()) => debug_log!(
                    "[DeskLink] Text focus sent to {device_id}: editable {editable}"
                ),
                Err(error) => debug_log!(
                    "[DeskLink] Text focus (editable {editable}) not sent to {device_id}: {error}"
                ),
            }
        })
    });

    let mut held_inputs = std::collections::HashSet::<(bool, i32)>::new();
    let mut portal_revoked = false;
    let mut no_video = false;
    // Why the desk ended the share, when it did: the phone is told.
    let mut stopped_here: Option<String> = None;
    let mut last_frames = 0u64;
    let mut last_progress = std::time::Instant::now();
    let mut restarts = 0u32;
    let mut last_rebuild: Option<std::time::Instant> = None;
    let mut scroll_open: Option<std::time::Instant> = None;
    'serve: loop {
        // A scroll gesture ends when its steps stop coming.
        if scroll_open.is_some_and(|last| last.elapsed() >= SCROLL_FINISH_AFTER) {
            let _ = portal.pointer_axis(0.0, 0.0, true);
            scroll_open = None;
        }
        // Capture watchdog; see `CAPTURE_STALL`.
        let frames = peer.desktop_screen_frames_sent();
        match capture_check(
            frames != last_frames,
            peer.media_ready() && !is_dozing(&device_id),
            last_progress.elapsed(),
            last_rebuild.map(|at| at.elapsed()),
            restarts,
        ) {
            CaptureCheck::Flowing { reset } => {
                last_frames = frames;
                last_progress = std::time::Instant::now();
                if reset {
                    restarts = 0;
                }
            }
            CaptureCheck::Wait => {}
            CaptureCheck::Hold => last_progress = std::time::Instant::now(),
            CaptureCheck::GiveUp => {
                no_video = true;
                break;
            }
            CaptureCheck::Rebuild => {
                restarts += 1;
                debug_log!(
                "[DeskLink] Desktop share to {device_id} produced no video for {:?}; rebuilding the capture ({restarts} of {CAPTURE_RESTARTS})",
                CAPTURE_STALL
            );
                if let Err(error) = capture(portal.active()) {
                    eprintln!("[DeskLink] Could not rebuild the desktop capture: {error}");
                }
                last_frames = peer.desktop_screen_frames_sent();
                last_progress = std::time::Instant::now();
                last_rebuild = Some(last_progress);
            }
        }
        let wait = if scroll_open.is_some() {
            SCROLL_FINISH_AFTER
        } else {
            std::time::Duration::from_millis(500)
        };
        match receiver.recv_timeout(wait) {
            Ok(DesktopPortalCommand::Input(mut actions)) => {
                // Each portal call is a blocking D-Bus round trip, while touch
                // motion arrives at 120-240 Hz: one call per event let the
                // cursor fall ever further behind the finger. Take everything
                // queued meanwhile and merge consecutive motion.
                let mut stop_after = false;
                let mut switch_after = false;
                loop {
                    match receiver.try_recv() {
                        Ok(DesktopPortalCommand::Input(more)) => actions.extend(more),
                        Ok(DesktopPortalCommand::SwitchScreen) => switch_after = true,
                        Ok(DesktopPortalCommand::Stop(reason)) => {
                            stopped_here = reason;
                            stop_after = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                // Positions go in the screen's real pixels; learn them from the
                // capture once it has negotiated (see `pointer_move`).
                if !portal.knows_active_pixels() {
                    if let Some(size) = peer.desktop_screen_source_size() {
                        debug_log!(
                            "[DeskLink] Phone input mapped onto {}x{} screen pixels",
                            size.0,
                            size.1
                        );
                        portal.set_active_pixels(size);
                    }
                }
                if actions
                    .iter()
                    .any(|action| matches!(action, PortalInputAction::Scroll(..)))
                {
                    scroll_open = Some(std::time::Instant::now());
                }
                for action in coalesce_motion(actions) {
                    let held = match &action {
                        PortalInputAction::Button { evdev_code, .. } => Some((false, *evdev_code)),
                        PortalInputAction::Key { keysym, .. } => Some((true, *keysym)),
                        _ => None,
                    };
                    let pressed = match &action {
                        PortalInputAction::Button { pressed, .. }
                        | PortalInputAction::Key { pressed, .. } => Some(*pressed),
                        _ => None,
                    };
                    let described = format!("{action:?}");
                    if let Err(error) = portal.apply(action) {
                        eprintln!(
                            "[DeskLink] Portal input dispatch failed for {described}: {error}"
                        );
                        // A revoked session is what makes every call fail; without
                        // this check it is noticed only after 500 ms of idle, which
                        // continuous motion never allows.
                        if portal.is_session_closed() {
                            portal_revoked = true;
                            break 'serve;
                        }
                        break;
                    }
                    if let (Some(held), Some(pressed)) = (held, pressed) {
                        if pressed && !held.0 {
                            if let Some(watcher) = &text_focus {
                                watcher.clicked();
                            }
                        }
                        if pressed {
                            held_inputs.insert(held);
                        } else {
                            held_inputs.remove(&held);
                        }
                    }
                }
                // Held keys and buttons are released whenever the phone no longer
                // holds input — not only when a lease *expires*: a pause, a stale
                // heartbeat, a replaced request all clear the lease first, and the
                // expiry check then sees nothing to release. A drag would leave
                // the mouse button stuck down.
                if !held_inputs.is_empty() && !peer_input_active(&device_id) {
                    release_held_portal_inputs(&*portal, &mut held_inputs);
                }
                if switch_after {
                    switch_screen();
                }
                if stop_after {
                    break;
                }
            }
            Ok(DesktopPortalCommand::SwitchScreen) => switch_screen(),
            Ok(DesktopPortalCommand::Stop(reason)) => {
                stopped_here = reason;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if portal.is_session_closed() {
                    portal_revoked = true;
                    break;
                }
                if !held_inputs.is_empty() && !peer_input_active(&device_id) {
                    release_held_portal_inputs(&*portal, &mut held_inputs);
                }
                // Let a silent phone-owned lease lapse.
                if let Ok(mut open) = remote_session::sessions().lock() {
                    if let Some(session) = open.get_mut(&device_id) {
                        session.expire_peer_input_lease(now_millis());
                    }
                }
            }
        }
    }
    release_held_portal_inputs(&*portal, &mut held_inputs);
    let told = if no_video {
        Some(STOPPED_SENDING.to_string())
    } else if portal_revoked {
        Some("Screen sharing was stopped on the computer".to_string())
    } else {
        stopped_here
    };
    if let Some(reason) = told {
        let sequence = remote_session::sessions()
            .lock()
            .ok()
            .and_then(|mut sessions| {
                sessions.get_mut(&device_id).map(|session| {
                    // Only this share's session: a newer one may already be
                    // running for the same phone.
                    session.stop_if_current(&session_id);
                    session.next_sequence()
                })
            })
            .unwrap_or(1);
        let stopped = screen_stopped(&attempt_id, &wire, &session_id, sequence, reason);
        let _ = send_remote_session_message(&peer, &wire, &stopped);
    }
    portal.release_all();
    // `_cleanup` removes this share's registry entry and stops its capture.
}

/// The `screen-error` that tells the phone its view of this screen ended,
/// and why.
pub(super) fn screen_stopped(
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    session_id: &str,
    sequence: i64,
    reason: String,
) -> remote_session::ControlMessage {
    let mut stopped = remote_session::ControlMessage::new(
        remote_session::ControlKind::ScreenError,
        attempt_id,
        wire,
        session_id,
        sequence,
        now_millis(),
    );
    stopped.reason =
        Some(crate::device_links::webrtc::remote_desktop_portal::plain_portal_error(&reason));
    stopped
}
