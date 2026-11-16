//! Tests of the coordinator: signalling and handover decisions, recovery and
//! liveness rules, the screen share's approvals and capture health, the
//! worker guard, files over a peer and ICE configuration. No real peer: the
//! decisions are pure functions or driven with fakes.

#[test]
fn a_failed_write_closes_the_socket_but_closed_link_errors_do_not() {
    use crate::device_links::core::SessionLink;
    let link = SessionLink::test_placeholder("phone");
    super::close_link_after_failed_write(&link, "Session link is closed");
    assert!(!link.is_closed());
    super::close_link_after_failed_write(&link, "Broken pipe (os error 32)");
    assert!(
        link.is_closed(),
        "a dead socket is closed so the redialer replaces it"
    );
}

/// The capture watchdog: a quiet stream is rebuilt every
/// `CAPTURE_STALL`, up to `CAPTURE_RESTARTS` times in a row (~15 s,
/// longer than a WirePlumber restart), then the share is given up; only
/// video that keeps flowing after a rebuild clears the count.
#[test]
fn a_stalled_capture_is_rebuilt_until_it_flows_or_runs_out_of_tries() {
    use super::{capture_check, CaptureCheck, CAPTURE_RESTARTS, CAPTURE_STALL};
    use std::time::Duration;
    let s = Duration::from_secs;
    assert_eq!(
        capture_check(false, true, s(1), None, 0),
        CaptureCheck::Wait
    );
    assert_eq!(
        capture_check(false, true, CAPTURE_STALL, None, 0),
        CaptureCheck::Rebuild
    );
    // A WirePlumber restart: the stream dies mid-share, every rebuild is
    // tried in turn, and the fifth failure ends it.
    let mut restarts = 0;
    let mut rebuilds = 0;
    loop {
        match capture_check(false, true, CAPTURE_STALL, Some(CAPTURE_STALL), restarts) {
            CaptureCheck::Rebuild => {
                restarts += 1;
                rebuilds += 1;
            }
            CaptureCheck::GiveUp => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(rebuilds, CAPTURE_RESTARTS);
    assert!(
        CAPTURE_STALL * CAPTURE_RESTARTS >= s(15),
        "outlasts a WirePlumber restart"
    );
    // Picture back after a rebuild: the count is kept until it has
    // flowed a while, then cleared.
    assert_eq!(
        capture_check(true, true, s(0), Some(s(2)), 2),
        CaptureCheck::Flowing { reset: false }
    );
    assert_eq!(
        capture_check(true, true, s(0), Some(s(11)), 2),
        CaptureCheck::Flowing { reset: true }
    );
    assert_eq!(
        capture_check(true, true, s(0), None, 0),
        CaptureCheck::Flowing { reset: true }
    );
    // The link not ready (ICE/DTLS slow, a blip, the phone dozing for its
    // 35 s): a healthy capture held by webrtcbin is neither rebuilt nor
    // given up, however long, even with every rebuild already spent.
    for quiet in [CAPTURE_STALL, s(35), s(120)] {
        assert_eq!(
            capture_check(false, false, quiet, None, 0),
            CaptureCheck::Hold
        );
        assert_eq!(
            capture_check(false, false, quiet, Some(quiet), CAPTURE_RESTARTS),
            CaptureCheck::Hold
        );
    }
    assert_eq!(
        capture_check(true, false, s(0), None, 0),
        CaptureCheck::Flowing { reset: true },
        "video that moves is video"
    );
}

/// A capture that fails to start (WirePlumber restarting under it) is
/// tried again, but not for ever and not once the share is cancelled.
#[test]
fn a_capture_that_fails_to_start_is_tried_again_a_few_times() {
    use std::sync::atomic::AtomicBool;
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    let result = super::start_capture_retrying(
        &cancel,
        |_| {},
        |_| {
            calls += 1;
            if calls < 3 {
                Err("sync".into())
            } else {
                Ok(())
            }
        },
    );
    assert_eq!((result, calls), (Ok(()), 3));

    let mut calls = 0;
    let result = super::start_capture_retrying(
        &cancel,
        |_| {},
        |_| {
            calls += 1;
            Err(format!("attempt {calls}"))
        },
    );
    assert_eq!(result, Err("attempt 4".to_string()));
    assert_eq!(calls, super::CAPTURE_START_ATTEMPTS);

    cancel.store(true, std::sync::atomic::Ordering::Release);
    let mut calls = 0;
    let result = super::start_capture_retrying(
        &cancel,
        |_| {},
        |_| {
            calls += 1;
            Err("sync".to_string())
        },
    );
    assert!(result.is_err());
    assert_eq!(calls, 1, "a cancelled share is not retried");

    // Released or replaced while it paused: no attempt after the pause,
    // and every attempt after the first is the owner-checked one.
    let cancel = AtomicBool::new(false);
    let mut attempts = Vec::new();
    let result = super::start_capture_retrying(
        &cancel,
        |_| cancel.store(true, std::sync::atomic::Ordering::Release),
        |attempt| {
            attempts.push(attempt);
            Err("sync".to_string())
        },
    );
    assert!(result.is_err());
    assert_eq!(
        attempts,
        vec![1],
        "nothing is started after a cancelling pause"
    );
    let cancel = AtomicBool::new(false);
    let mut attempts = Vec::new();
    let _ = super::start_capture_retrying(
        &cancel,
        |_| {},
        |attempt| {
            attempts.push(attempt);
            Err("sync".to_string())
        },
    );
    assert_eq!(attempts, vec![1, 2, 3, 4]);
}

/// A share that failed is said at the desk too, except a refusal made there.
#[test]
fn a_failed_share_is_told_at_the_desk_unless_declined_there() {
    use super::super::remote_desktop_portal::plain_portal_error;
    assert!(!super::tell_desk_about_failure(&plain_portal_error(
        "Desktop permission request was denied"
    )));
    assert!(!super::tell_desk_about_failure(
        super::super::remote_desktop_portal::LOCKED_HERE
    ));
    assert!(super::tell_desk_about_failure(&plain_portal_error(
        "Could not attach video to the negotiated WebRTC screen track"
    )));
    assert!(super::tell_desk_about_failure(&plain_portal_error(
        "Could not start DeskLink screen capture: Failed to sync state with parent"
    )));
}

#[test]
fn a_phone_awaits_approval_until_its_own_share_answers() {
    use super::{portal_approvals, waited_long_enough, AwaitingApproval, APPROVAL_NOTICE_AFTER};
    let phone = "approval-test-phone";
    let listed_after = |wait: std::time::Duration| {
        let waiting = portal_approvals().lock().unwrap();
        waited_long_enough(
            &waiting,
            std::time::Instant::now() + wait,
            APPROVAL_NOTICE_AFTER,
        )
        .iter()
        .any(|id| id == phone)
    };
    assert!(!listed_after(std::time::Duration::from_secs(60)));
    let first = AwaitingApproval::begin(phone, 1);
    assert!(listed_after(std::time::Duration::from_secs(60)));
    // A retry replaces the share; the old one ending must not clear the new one's notice.
    let second = AwaitingApproval::begin(phone, 2);
    drop(first);
    assert!(listed_after(std::time::Duration::from_secs(60)));
    drop(second);
    assert!(!listed_after(std::time::Duration::from_secs(60)));
}

/// A remembered grant answers at once: nothing is posted for a wait shorter than the threshold.
#[test]
fn the_notice_waits_for_the_portal_to_take_a_moment() {
    use super::{waited_long_enough, APPROVAL_NOTICE_AFTER};
    use std::time::{Duration, Instant};
    let since = Instant::now();
    let waiting = std::collections::HashMap::from([("phone".to_string(), (1u64, since))]);
    let at = |extra: Duration| waited_long_enough(&waiting, since + extra, APPROVAL_NOTICE_AFTER);
    assert!(at(Duration::ZERO).is_empty());
    assert!(at(APPROVAL_NOTICE_AFTER - Duration::from_millis(1)).is_empty());
    assert_eq!(at(APPROVAL_NOTICE_AFTER), vec!["phone".to_string()]);
    assert_eq!(APPROVAL_NOTICE_AFTER, Duration::from_millis(1500));
}

/// An unpaired phone leaves no liveness behind (the maps would only grow).
#[test]
fn unpairing_forgets_what_was_heard_from_a_phone() {
    use super::{data_silence, forget_liveness, note_data_inbound};
    let id = format!("forgotten-{}", uuid::Uuid::new_v4());
    note_data_inbound(&id);
    note_heartbeat(&id, &{
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
        packet.set("dozing", true);
        packet
    });
    assert!(data_silence(&id).is_some() && is_dozing(&id));
    forget_liveness(&id);
    assert!(data_silence(&id).is_none() && !is_dozing(&id));
}

use super::{
    apply_own_denial, authorize_desktop_remote_input, coalesce_motion, desktop_capture_needed,
    desktop_portal_services, effective_silence, end_screen_session, ensure_phone_view_allowed,
    expired_request_action, is_dozing, liveness, note_heartbeat, offer_attempt_disposition,
    recovery_delay, remote_session, remove_service_if_current, reports_dozing, screen_stopped,
    stop_desktop_portal_session, take_probe_now, DesktopPortalCommand, DesktopPortalService,
    ExpiredRequest, Liveness, OfferAttemptDisposition, RECOVERY_BACKOFF_MILLIS,
    SHARING_STOPPED_AT_DESK,
};
use crate::device_links::config::Config;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::webrtc::remote_session::SessionState;
use crate::device_links::webrtc::remote_session::{
    ControlKind, ControlMessage, RemoteSession, ScreenDirection,
};
use crate::device_links::webrtc::WebRtcWireBinding;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

fn register_share(
    device: &str,
    id: u64,
) -> (Arc<AtomicBool>, mpsc::Receiver<DesktopPortalCommand>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(super::PORTAL_QUEUE);
    let (_, done) = mpsc::channel();
    desktop_portal_services().lock().unwrap().insert(
        device.to_string(),
        DesktopPortalService {
            id,
            sender,
            can_control: false,
            extended: false,
            cancel: Arc::clone(&cancel),
            done,
        },
    );
    (cancel, receiver)
}

/// A worker that is only now finishing must not remove the entry of the
/// share that replaced it — that ended the new capture right after the user
/// approved it.
#[test]
fn a_finishing_worker_removes_only_its_own_registry_entry() {
    let device = "test-device-own-entry";
    let _ = register_share(device, 41);
    let _ = register_share(device, 42); // a newer share for the same phone

    assert!(!remove_service_if_current(device, 41), "41 was replaced");
    assert!(desktop_portal_services()
        .lock()
        .unwrap()
        .contains_key(device));

    assert!(remove_service_if_current(device, 42));
    assert!(!desktop_portal_services()
        .lock()
        .unwrap()
        .contains_key(device));
}

/// A worker still waiting on the GNOME dialog never reads the `Stop`
/// command, so stopping must cancel it as well.
#[test]
fn stopping_a_share_cancels_a_worker_waiting_on_the_consent_dialog() {
    let device = "test-device-cancel";
    let (cancel, receiver) = register_share(device, 7);

    stop_desktop_portal_session(device, None);

    assert!(cancel.load(Ordering::Acquire));
    assert!(matches!(
        receiver.try_recv(),
        Ok(DesktopPortalCommand::Stop(None))
    ));
    assert!(!desktop_portal_services()
        .lock()
        .unwrap()
        .contains_key(device));
}

/// "Stop Sharing" at the desk tells the phone, so it leaves the desk
/// screen instead of waiting on a frozen picture.
#[test]
fn stopping_a_share_at_the_desk_tells_the_phone_why() {
    let device = "test-device-stop-told";
    let (_cancel, receiver) = register_share(device, 9);
    end_screen_session(device);
    assert!(matches!(
        receiver.try_recv(),
        Ok(DesktopPortalCommand::Stop(Some(reason))) if reason == SHARING_STOPPED_AT_DESK
    ));

    let wire = WebRtcWireBinding {
        sender_device_id: "desk".into(),
        peer_device_id: device.into(),
        session_id: 3,
        generation: 1,
    };
    let message = screen_stopped(
        "attempt",
        &wire,
        "remote-1",
        4,
        SHARING_STOPPED_AT_DESK.to_string(),
    );
    assert_eq!(message.kind, remote_session::ControlKind::ScreenError);
    assert_eq!(message.reason.as_deref(), Some(SHARING_STOPPED_AT_DESK));
    assert_eq!(message.remote_session_id, "remote-1");
}

/// One session per device serves both directions. Starting to view the
/// phone while the desktop is being shared would overwrite the live share's
/// session id while its capture keeps streaming.
#[test]
fn viewing_the_phone_is_refused_while_the_desktop_is_being_shared() {
    let device = "test-device-conflict";
    assert!(ensure_phone_view_allowed(device).is_ok());

    let _ = register_share(device, 9);
    assert!(ensure_phone_view_allowed(device).is_err());

    stop_desktop_portal_session(device, None);
    assert!(ensure_phone_view_allowed(device).is_ok());
}

#[test]
fn an_expired_view_request_is_cleaned_up_on_the_side_that_owns_it() {
    assert_eq!(
        expired_request_action(
            SessionState::RequestingView,
            Some(ScreenDirection::DesktopToPhone)
        ),
        ExpiredRequest::CancelDesktopShare
    );
    assert_eq!(
        expired_request_action(
            SessionState::RequestingView,
            Some(ScreenDirection::PhoneToDesktop)
        ),
        ExpiredRequest::ReleasePhoneView
    );
    // A control request that timed out only falls back to the view.
    assert_eq!(
        expired_request_action(
            SessionState::RequestingControl,
            Some(ScreenDirection::PhoneToDesktop)
        ),
        ExpiredRequest::Nothing
    );
}

// ── File transfers over real connections ────────────────────────────

/// Two real peers (ICE, DTLS, SCTP over loopback), each with a transfer
/// manager attached exactly as the coordinator attaches one, and the
/// event routing the coordinator's worker does for file traffic: control
/// envelopes to `on_control`, binary frames to `on_frame`. Heartbeats and a
/// "terminal" echo ride their own channels so a test can see whether a
/// transfer delays them.
struct LiveRig {
    pair_offerer: Arc<DesktopWebRtcPeer>,
    pair_answerer: Arc<DesktopWebRtcPeer>,
    offerer: Arc<WebRtcTransferManager>,
    answerer: Arc<WebRtcTransferManager>,
    offerer_done: mpsc::Receiver<Completed>,
    answerer_done: mpsc::Receiver<Completed>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Longest round trip seen on the liveness and terminal channels.
    slowest: Arc<Mutex<(Duration, Duration)>>,
    base: std::path::PathBuf,
    // Kept alive: its drop is the test's watchdog.
    _watchdog: Box<dyn std::any::Any>,
}

use crate::device_links::webrtc::file_protocol::FileControl;
use crate::device_links::webrtc::peer_connection::DesktopWebRtcPeer;
use crate::device_links::webrtc::transfer_manager::{Completed, WebRtcTransferManager};
use crate::device_links::webrtc::{WebRtcChannel, WebRtcEnvelope};
use std::time::{Duration, Instant};

impl LiveRig {
    fn new(label: &str, base: std::path::PathBuf) -> Self {
        Self::through(label, base, None)
    }

    /// The same over a [`crate::device_links::webrtc::chaos::Chaos`]
    /// network. The state and download folders are named after the
    /// side, so a rig built again on `base` is the same two devices
    /// after a reconnect.
    fn through(
        label: &str,
        base: std::path::PathBuf,
        chaos: Option<&Arc<crate::device_links::webrtc::chaos::Chaos>>,
    ) -> Self {
        use crate::device_links::webrtc::peer_connection::tests as net;
        use crate::device_links::webrtc::PeerEvent;
        let pair = match chaos {
            Some(chaos) => net::loopback_through(chaos),
            None => net::loopback(),
        };
        let all = [
            WebRtcChannel::Control,
            WebRtcChannel::Events,
            WebRtcChannel::FileControl,
            WebRtcChannel::FileData,
            WebRtcChannel::Terminal,
            WebRtcChannel::Liveness,
        ];
        net::wait_for_channels(&pair.offerer_events, &all);
        net::wait_for_channels(&pair.answerer_events, &all);
        let wire_o = WebRtcWireBinding {
            sender_device_id: "offerer".into(),
            peer_device_id: "answerer".into(),
            session_id: 11,
            generation: 1,
        };
        let wire_a = WebRtcWireBinding {
            sender_device_id: "answerer".into(),
            peer_device_id: "offerer".into(),
            session_id: 11,
            generation: 1,
        };
        let _ = label;
        std::fs::create_dir_all(&base).unwrap();
        let make = |side: &str| {
            Arc::new(
                WebRtcTransferManager::new(
                    base.join(format!("{side}-state")),
                    base.join(format!("{side}-dl")),
                )
                .unwrap(),
            )
        };
        let (offerer, answerer) = (make("o"), make("a"));
        let (tx_o, offerer_done) = mpsc::channel();
        let (tx_a, answerer_done) = mpsc::channel();
        let (tx_o, tx_a) = (Mutex::new(tx_o), Mutex::new(tx_a));
        offerer.set_completion_hook(move |c| {
            let _ = tx_o.lock().unwrap().send(c.clone());
        });
        answerer.set_completion_hook(move |c| {
            let _ = tx_a.lock().unwrap().send(c.clone());
        });
        super::attach_transfer_manager(&offerer, &pair.offerer, &wire_o, None);
        super::attach_transfer_manager(&answerer, &pair.answerer, &wire_a, None);
        let stop = Arc::new(AtomicBool::new(false));
        let slowest = Arc::new(Mutex::new((Duration::ZERO, Duration::ZERO)));
        let epoch = Instant::now();
        let route = |peer: Arc<DesktopWebRtcPeer>,
                     manager: Arc<WebRtcTransferManager>,
                     wire: WebRtcWireBinding,
                     events: mpsc::Receiver<PeerEvent>| {
            let (stop, slowest) = (Arc::clone(&stop), Arc::clone(&slowest));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let Ok(event) = events.recv_timeout(Duration::from_millis(5)) else {
                        continue;
                    };
                    match event {
                        PeerEvent::Envelope {
                            channel: WebRtcChannel::FileControl,
                            bytes,
                            ..
                        } => {
                            let envelope = WebRtcEnvelope::from_json(&bytes).unwrap();
                            let payload = envelope.validate(&wire).unwrap();
                            manager.on_control(FileControl::from_json(&payload).unwrap());
                        }
                        PeerEvent::Binary {
                            channel: WebRtcChannel::FileData,
                            bytes,
                            ..
                        } => manager.on_frame(bytes),
                        // A probe goes back at once; an answer is timed.
                        PeerEvent::Envelope {
                            channel: channel @ (WebRtcChannel::Liveness | WebRtcChannel::Terminal),
                            bytes,
                            ..
                        } => {
                            let text = String::from_utf8_lossy(&bytes).into_owned();
                            if let Some(sent) = text.strip_prefix("ping:") {
                                let stamp = sent.split(':').next().unwrap_or(sent);
                                let _ = peer.send_text(channel, &format!("pong:{stamp}"));
                            } else if let Some(sent) = text.strip_prefix("pong:") {
                                let sent: u128 = sent
                                    .split(':')
                                    .next()
                                    .and_then(|n| n.parse().ok())
                                    .unwrap_or(0);
                                let took = epoch
                                    .elapsed()
                                    .saturating_sub(Duration::from_nanos(sent as u64));
                                let mut slowest = slowest.lock().unwrap();
                                if channel == WebRtcChannel::Liveness {
                                    slowest.0 = slowest.0.max(took);
                                } else {
                                    slowest.1 = slowest.1.max(took);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            })
        };
        let threads = vec![
            route(
                Arc::clone(&pair.offerer),
                Arc::clone(&offerer),
                wire_o.clone(),
                pair.offerer_events,
            ),
            route(
                Arc::clone(&pair.answerer),
                Arc::clone(&answerer),
                wire_a.clone(),
                pair.answerer_events,
            ),
        ];
        // Heartbeats and terminal traffic from the offerer, every 20 ms.
        let (peer, stop_pings) = (Arc::clone(&pair.offerer), Arc::clone(&stop));
        let pinger = std::thread::spawn(move || {
            while !stop_pings.load(Ordering::Acquire) {
                let now = epoch.elapsed().as_nanos();
                let _ = peer.send_heartbeat(&format!("ping:{now}"));
                let _ = peer.send_text(
                    WebRtcChannel::Terminal,
                    &format!("ping:{now}:{}", "x".repeat(2_000)),
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut threads = threads;
        threads.push(pinger);
        Self {
            pair_offerer: pair.offerer,
            pair_answerer: pair.answerer,
            offerer,
            answerer,
            offerer_done,
            answerer_done,
            stop,
            threads,
            slowest,
            base,
            _watchdog: Box::new(()),
        }
    }

    fn finished(self) {
        let base = self.base.clone();
        self.cut();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The connection is lost, as the coordinator's worker loses it:
    /// the managers are detached and stopped, the peers closed. What is
    /// on disk stays.
    fn cut(self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads {
            let _ = thread.join();
        }
        for manager in [&self.offerer, &self.answerer] {
            manager.detach();
            manager.shut_down();
        }
        self.pair_offerer.close();
        self.pair_answerer.close();
    }
}

fn live_base(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("desklink-live-{label}-{}", std::process::id()))
}

fn sha_of_file(path: &std::path::Path) -> String {
    use std::io::Read;
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = openssl::sha::Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    hasher.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// A file of `length` bytes that does not repeat on a block boundary.
fn write_pattern_file(path: &std::path::Path, length: u64, seed: u64) {
    use std::io::Write;
    let mut file = std::fs::File::create(path).unwrap();
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut buffer = vec![0u8; 1 << 20];
    let mut left = length;
    while left > 0 {
        for chunk in buffer.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        let take = left.min(buffer.len() as u64) as usize;
        file.write_all(&buffer[..take]).unwrap();
        left -= take as u64;
    }
    file.sync_all().unwrap();
}

/// Sends `sizes` (bytes) at once from the offerer or the answerer and
/// checks every file arrives byte for byte; the seconds it took, from the
/// first start to the last file placed.
fn live_transfer(label: &str, offerer_sends: bool, sizes: &[u64], base: std::path::PathBuf) -> f64 {
    let rig = LiveRig::new(label, base.clone());
    let (sender, receiver, done) = if offerer_sends {
        (&rig.offerer, &rig.answerer, &rig.answerer_done)
    } else {
        (&rig.answerer, &rig.offerer, &rig.offerer_done)
    };
    let _ = receiver;
    let sources: Vec<std::path::PathBuf> = sizes
        .iter()
        .enumerate()
        .map(|(index, size)| {
            let path = base.join(format!("source-{index}.bin"));
            write_pattern_file(&path, *size, index as u64 + 1);
            path
        })
        .collect();
    let started = Instant::now();
    for source in &sources {
        sender.start_send(source).unwrap();
    }
    let mut arrived = Vec::new();
    for _ in &sources {
        arrived.push(
            done.recv_timeout(Duration::from_secs(110))
                .expect("a file never arrived"),
        );
    }
    let seconds = started.elapsed().as_secs_f64();
    for source in &sources {
        let name = source.file_name().unwrap();
        let got = arrived
            .iter()
            .find(|completed| completed.path.file_name() == Some(name))
            .expect("every file arrives under its own name");
        assert_eq!(
            std::fs::metadata(&got.path).unwrap().len(),
            std::fs::metadata(source).unwrap().len()
        );
        assert_eq!(
            sha_of_file(&got.path),
            sha_of_file(source),
            "{name:?} arrived corrupted"
        );
    }
    rig.finished();
    seconds
}

/// When [`transfer_through_cuts`] cuts the connection.
enum CutWhen {
    /// After this much connected time (the soak row; timing-dependent).
    Every(Duration),
    /// Once the receiver holds this many more verified blocks than at
    /// the last cut: every cut lands mid-file whatever the machine's
    /// load, and each reconnect must make real progress.
    AfterBlocks(u64),
}

/// A file sent over a network that is cut again and again: each cut is
/// a black hole and then the end of the connection, the way a lost
/// Wi-Fi ends it; each reconnect is new peers and new managers on the
/// same disks, the way the coordinator rebuilds. The file arrives
/// whole, and every reconnect goes on from the blocks already verified.
fn transfer_through_cuts(
    label: &str,
    base: std::path::PathBuf,
    size: u64,
    when: CutWhen,
    at_most: u32,
    faults: crate::device_links::webrtc::chaos::Faults,
) {
    // A reconnect that makes no progress for this long is stuck (it
    // takes about a second on a loaded machine).
    const STUCK: Duration = Duration::from_secs(120);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let source = base.join("cut.bin");
    write_pattern_file(&source, size, 77);
    let chaos = crate::device_links::webrtc::chaos::Chaos::new(5);
    chaos.set(faults);
    let mut rig = LiveRig::through(label, base.clone(), Some(&chaos));
    rig.offerer.start_send(&source).unwrap();
    let (mut cuts, started) = (0u32, Instant::now());
    // Verified blocks held when the connection was last cut.
    let mut held_at_cut = 0u64;
    let completed = loop {
        let round = Instant::now();
        let mut resumed_checked = cuts == 0;
        let due = loop {
            if let Ok(completed) = rig.answerer_done.recv_timeout(Duration::from_millis(20)) {
                break Err(completed);
            }
            let verified = rig.answerer.verified_incoming_blocks();
            // The first sight of the transfer after a reconnect: it
            // goes on from what was verified, never from the start.
            if !resumed_checked && verified > 0 {
                assert!(
                    verified >= held_at_cut,
                    "cut {cuts}: resumed at block {verified}, below the {held_at_cut} verified"
                );
                resumed_checked = true;
            }
            let cut_now = match when {
                CutWhen::Every(every) => round.elapsed() >= every,
                CutWhen::AfterBlocks(blocks) => verified >= held_at_cut + blocks,
            };
            if cut_now {
                break Ok(verified);
            }
            assert!(
                round.elapsed() < STUCK,
                "no progress past {verified} blocks in {STUCK:?} after cut {cuts}"
            );
        };
        let verified = match due {
            Err(completed) => break completed,
            Ok(verified) => verified,
        };
        held_at_cut = held_at_cut.max(verified);
        cuts += 1;
        assert!(cuts <= at_most, "still not there after {cuts} cuts");
        chaos.blackhole(true);
        std::thread::sleep(Duration::from_millis(300));
        rig.cut();
        chaos.blackhole(false);
        rig = LiveRig::through(label, base.clone(), Some(&chaos));
        // The coordinator resumes on feature-ready.
        rig.offerer.resume_sends();
        eprintln!(
            "[cuts] {label}: cut {cuts} at {:?}, {verified} blocks verified",
            started.elapsed()
        );
    };
    assert_eq!(std::fs::metadata(&completed.path).unwrap().len(), size);
    assert_eq!(
        sha_of_file(&completed.path),
        sha_of_file(&source),
        "arrived corrupted"
    );
    assert!(cuts >= 1, "the transfer was never cut: make it bigger");
    eprintln!(
        "[cuts] {label}: {size} bytes through {cuts} cuts in {:?}",
        started.elapsed()
    );
    rig.finished();
}

/// The scenario row "a transfer with cuts", at test-suite size: cut
/// each time 6 more blocks are verified, so every cut lands mid-file
/// however loaded the machine is, and every reconnect has to resume.
#[test]
fn a_transfer_cut_mid_file_resumes_and_arrives_whole() {
    let size = 48u64 << 20;
    let step = 6;
    transfer_through_cuts(
        "cut-small",
        live_base("cut-small"),
        size,
        CutWhen::AfterBlocks(step),
        (size >> 20) as u32 / step as u32 + 1,
        Default::default(),
    );
}

/// The full row: 300 MiB, cut every ten seconds (on disk, not in /tmp),
/// over a Wi-Fi-like path (20 ms round trip, 0.5 % loss) so it takes
/// long enough to be cut several times. (Not with jitter: packets
/// overtaking each other by up to 10 ms brought SCTP down to about
/// 0.3 MB/s, which says more about usrsctp and reordering than about
/// resuming; see docs/testing/CONNECTION-HARDENING.md.)
#[test]
#[ignore = "300 MiB; run with --ignored chaos"]
fn chaos_a_300_mib_transfer_cut_every_ten_seconds_arrives_whole() {
    transfer_through_cuts(
        "cut-300",
        bench_base("cut-300"),
        300 << 20,
        CutWhen::Every(Duration::from_secs(10)),
        60,
        crate::device_links::webrtc::chaos::Faults {
            loss: 0.005,
            delay: Duration::from_millis(10),
            ..Default::default()
        },
    );
}

#[test]
fn a_file_crosses_a_live_connection_from_the_offerer() {
    live_transfer(
        "from-offerer",
        true,
        &[2 * 1024 * 1024 + 123],
        live_base("from-offerer"),
    );
}

#[test]
fn a_file_crosses_a_live_connection_from_the_answerer() {
    live_transfer(
        "from-answerer",
        false,
        &[2 * 1024 * 1024 + 123],
        live_base("from-answerer"),
    );
}

#[test]
fn an_empty_and_a_one_byte_file_cross_a_live_connection() {
    live_transfer("tiny", true, &[0, 1], live_base("tiny"));
}

/// The edge-case row "two big transfers plus Bucket sync plus a terminal
/// at once": two big files and a small one (queued, as a Bucket file is,
/// behind the two slots) move together with heartbeats on the liveness
/// channel and terminal traffic on its own: all complete, and the
/// heartbeat and terminal round trips stay far below the 5 s probe time
/// (about 100 ms on a quiet machine; the bound allows a loaded test run).
#[test]
fn two_big_transfers_a_third_and_live_traffic_all_progress_without_delaying_heartbeats() {
    let base = live_base("mixed");
    let rig = LiveRig::new("mixed", base.clone());
    let sizes = [48u64 << 20, 48 << 20, 3 << 20];
    let sources: Vec<std::path::PathBuf> = sizes
        .iter()
        .enumerate()
        .map(|(index, size)| {
            let path = base.join(format!("mixed-{index}.bin"));
            write_pattern_file(&path, *size, 100 + index as u64);
            path
        })
        .collect();
    for source in &sources {
        rig.offerer.start_send(source).unwrap();
    }
    let mut arrived = Vec::new();
    for _ in &sources {
        arrived.push(
            rig.answerer_done
                .recv_timeout(Duration::from_secs(110))
                .unwrap(),
        );
    }
    for source in &sources {
        let got = arrived
            .iter()
            .find(|c| c.path.file_name() == source.file_name())
            .unwrap();
        assert_eq!(sha_of_file(&got.path), sha_of_file(source));
    }
    let (liveness, terminal) = *rig.slowest.lock().unwrap();
    eprintln!("[mixed] slowest heartbeat {liveness:?}, slowest terminal echo {terminal:?}");
    assert!(liveness > Duration::ZERO, "heartbeats were answered");
    assert!(
        liveness < Duration::from_secs(4) && terminal < Duration::from_secs(4),
        "a transfer delayed the other channels: heartbeat {liveness:?}, terminal {terminal:?}"
    );
    rig.finished();
}

// ── Opt-in benchmarks: `cargo test --release -- --ignored transfer_throughput --nocapture`
//
// Real webrtcbin on both ends over loopback: they measure the protocol and
// the stack, not Wi-Fi. Files go under `$DESKLINK_BENCH_DIR` (default
// `~/.cache/desklink-bench`; not `/tmp`, which is RAM on most systems).

fn bench_base(label: &str) -> std::path::PathBuf {
    std::env::var_os("DESKLINK_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::cache_dir().map(|cache| cache.join("desklink-bench")))
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("{label}-{}", std::process::id()))
}

fn bench(label: &str, megabytes: &[u64]) {
    let sizes: Vec<u64> = megabytes.iter().map(|mb| mb * 1_000_000).collect();
    let total: u64 = sizes.iter().sum();
    let seconds = live_transfer(label, true, &sizes, bench_base(label));
    let rate = total as f64 / seconds.max(0.001) / 1e6;
    // A floor, for a regression in the sender (a lock, a sync in the hot
    // path): the v2 stack does about 110 MB/s on loopback. Over a shaped
    // link (see docs/testing/RELIABILITY.md) the network is the limit, so
    // set DESKLINK_BENCH_FLOOR=0 there.
    let floor: f64 = std::env::var("DESKLINK_BENCH_FLOOR")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(30.0);
    eprintln!(
        "[bench] {label}: {megabytes:?} MB in {seconds:.2} s = {:.1} MB/s",
        total as f64 / seconds / 1e6
    );
    assert!(
        rate >= floor,
        "{label}: {rate:.1} MB/s is under the floor of {floor} MB/s"
    );
}

#[test]
#[ignore = "benchmark; run with --ignored transfer_throughput"]
fn transfer_throughput_100mb_one_file() {
    bench("one-100mb", &[100]);
}

#[test]
#[ignore = "benchmark; run with --ignored transfer_throughput"]
fn transfer_throughput_100mb_two_files() {
    bench("two-100mb", &[50, 50]);
}

#[test]
#[ignore = "benchmark; run with --ignored transfer_throughput"]
fn transfer_throughput_1gb_one_file() {
    bench("one-1gb", &[1000]);
}

#[test]
#[ignore = "benchmark; run with --ignored transfer_throughput"]
fn transfer_throughput_1gb_two_files() {
    bench("two-1gb", &[500, 500]);
}

fn session_control(kind: ControlKind, session_id: &str) -> ControlMessage {
    ControlMessage {
        kind,
        session_attempt_id: "attempt".to_string(),
        device_id: "phone".to_string(),
        session_id: 1,
        connection_generation: 1,
        remote_session_id: session_id.to_string(),
        sequence: 1,
        timestamp: 1_000,
        lease_id: None,
        direction: Some(ScreenDirection::DesktopToPhone),
        owner_device_id: None,
        lease_expires_at: None,
        reason: None,
        screen_width: None,
        screen_height: None,
        screen_rotation: None,
        editable: None,
        screen_count: None,
        max_dimension: None,
        fps: None,
        password: None,
        action: None,
        audio_direction: None,
        source: None,
        sample_rate: None,
        channels: None,
        value: None,
        capture_kind: None,
        target: None,
        text: None,
        mime_type: None,
        pages: None,
        display: None,
    }
}

#[test]
fn malformed_input_does_not_refresh_the_peer_control_lease() {
    let mut session = RemoteSession::new();
    let session_id = session.begin_view(ScreenDirection::DesktopToPhone);
    session
        .accept(
            &session_control(ControlKind::ScreenReady, &session_id),
            1_001,
        )
        .unwrap();
    session
        .accept(
            &session_control(ControlKind::RequestControl, &session_id),
            1_002,
        )
        .unwrap();
    session
        .grant_peer_control("lease".to_string(), "phone".to_string(), 100_000)
        .unwrap();

    let mut packet = NetworkPacket::new("desklink.mousepad.request");
    packet.set(super::remote_session::FIELD_REMOTE_SESSION_ID, session_id);
    packet.set(super::remote_session::FIELD_LEASE_ID, "lease");
    packet.set(super::remote_session::FIELD_INPUT_SEQUENCE, 1i64);
    packet.set("x", 25);

    assert!(authorize_desktop_remote_input(&mut session, &packet, "phone", 5_000).is_err());
    assert_eq!(session.peer_input_lease().unwrap().expires_at, 100_000);
}

#[test]
fn a_denied_view_request_leaves_no_session_behind() {
    let device = "test-device-denied-view";
    remote_session::sessions()
        .lock()
        .unwrap()
        .entry(device.to_string())
        .or_default()
        .accept(&session_control(ControlKind::RequestView, "denied"), 1_000)
        .unwrap();
    assert_eq!(
        remote_session::sessions().lock().unwrap()[device].state(),
        SessionState::RequestingView
    );

    apply_own_denial(device, &session_control(ControlKind::ViewDenied, "denied"));

    let open = remote_session::sessions().lock().unwrap();
    assert_eq!(open[device].state(), SessionState::Stopped);
    assert_eq!(open[device].remote_session_id(), None);
}

#[test]
fn only_desktop_to_phone_requests_need_desktop_capture() {
    assert!(desktop_capture_needed(ScreenDirection::DesktopToPhone));
    assert!(!desktop_capture_needed(ScreenDirection::PhoneToDesktop));
}

/// The backoff used to clamp at its last rung and retry every 30 seconds
/// for the life of the process, so a connection that could never succeed
/// presented as "Connecting" forever. Exhausting the ladder must terminate
/// initiation so the UI can say something true.
#[test]
fn recovery_backoff_gives_up_instead_of_retrying_forever() {
    let delays: Vec<_> = (0..RECOVERY_BACKOFF_MILLIS.len() as u32)
        .map(|attempt| recovery_delay(attempt).expect("ladder rung"))
        .collect();

    // Strictly increasing, so each failure costs more than the last.
    assert!(delays.windows(2).all(|pair| pair[0] < pair[1]));

    let past_the_end = RECOVERY_BACKOFF_MILLIS.len() as u32;
    assert_eq!(recovery_delay(past_the_end), None);
    assert_eq!(recovery_delay(u32::MAX), None);
}

#[test]
fn motion_is_coalesced_without_crossing_clicks() {
    use super::super::remote_desktop_portal::PortalInputAction as A;
    let merged = coalesce_motion(vec![
        A::AbsoluteMotion(1.0, 1.0),
        A::AbsoluteMotion(2.0, 2.0),
        A::Button {
            evdev_code: 272,
            pressed: true,
        },
        A::RelativeMotion(1.0, 0.0),
        A::RelativeMotion(2.0, 3.0),
        A::Scroll(0.0, 1.0),
        A::Scroll(0.0, 1.0),
        A::Button {
            evdev_code: 272,
            pressed: false,
        },
    ]);
    assert_eq!(
        merged,
        vec![
            A::AbsoluteMotion(2.0, 2.0),
            A::Button {
                evdev_code: 272,
                pressed: true
            },
            A::RelativeMotion(3.0, 3.0),
            A::Scroll(0.0, 2.0),
            A::Button {
                evdev_code: 272,
                pressed: false
            },
        ]
    );
}

#[test]
fn liveness_probes_after_five_seconds_and_declares_dead_after_fifteen() {
    use std::time::Duration;
    let s = Duration::from_secs;
    assert_eq!(liveness(s(4), s(60), false), Liveness::Fine);
    assert_eq!(liveness(s(5), s(60), false), Liveness::Probe);
    // One probe per interval, not one per 500 ms worker tick.
    assert_eq!(liveness(s(7), s(2), false), Liveness::Fine);
    assert_eq!(liveness(s(14), s(5), false), Liveness::Probe);
    assert_eq!(liveness(s(15), s(0), false), Liveness::Dead);
}

#[test]
fn a_dozing_phone_gets_thirty_five_seconds_and_no_probes() {
    use std::time::Duration;
    let s = Duration::from_secs;
    assert_eq!(liveness(s(5), s(60), true), Liveness::Fine, "no probe");
    assert_eq!(liveness(s(20), s(60), true), Liveness::Fine, "past 15 s");
    assert_eq!(liveness(s(34), s(60), true), Liveness::Fine);
    assert_eq!(liveness(s(35), s(0), true), Liveness::Dead);
}

#[test]
fn a_peer_is_rebuilt_only_when_all_its_addresses_are_gone() {
    use super::addresses_vanished;
    let ip = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
    let present = [ip("192.168.1.20"), ip("fe80::1")];
    assert!(!addresses_vanished(&[ip("192.168.1.20")], &present));
    assert!(
        addresses_vanished(&[ip("10.0.0.5")], &present),
        "Wi-Fi switched"
    );
    assert!(
        !addresses_vanished(&[ip("10.0.0.5"), ip("fe80::1")], &present),
        "one left"
    );
    assert!(
        !addresses_vanished(&[], &present),
        "nothing recorded: nothing to judge"
    );
}

/// A peer never outlives its worker: the guard clears the session's
/// attempt even when the worker unwinds from a panic, and leaves a newer
/// attempt alone.
#[test]
fn the_worker_guard_clears_its_attempt_on_panic_but_not_a_newer_one() {
    use super::PeerWorkerGuard;
    use crate::device_links::core::test_link::TestNetwork;
    use crate::device_links::daemon::test_peer::peer_pair;
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "guard-desk", "guard-phone");
    let attempt = |desktop: &crate::device_links::daemon::test_peer::TestPeer| {
        desktop
            .sessions
            .with_session(&desktop.binding, |session| {
                session.webrtc_attempt_id.clone()
            })
            .unwrap()
    };
    let set = |id: &str| {
        desktop
            .sessions
            .with_session(&desktop.binding, |session| {
                session.webrtc_attempt_id = Some(id.to_string());
            })
            .unwrap();
    };
    set("a1");
    let (binding, sessions) = (desktop.binding.clone(), Arc::clone(&desktop.sessions));
    let worker = std::thread::spawn(move || {
        let _guard = PeerWorkerGuard {
            binding,
            attempt_id: "a1".into(),
            sessions,
        };
        panic!("the worker died");
    });
    assert!(worker.join().is_err());
    assert_eq!(attempt(&desktop), None, "cleared by the unwinding guard");

    // A newer attempt (a fresh offer) is not torn down by the old worker.
    set("a2");
    drop(PeerWorkerGuard {
        binding: desktop.binding.clone(),
        attempt_id: "a1".into(),
        sessions: Arc::clone(&desktop.sessions),
    });
    assert_eq!(attempt(&desktop).as_deref(), Some("a2"));
}

#[test]
fn the_recovery_ladder_is_the_shared_one() {
    assert_eq!(
        RECOVERY_BACKOFF_MILLIS,
        [250, 1_000, 2_000, 4_000, 8_000, 16_000]
    );
}

/// Feature-ready no longer resets the ladder: only a minute of healthy
/// ready time does (and a sign of the phone, below).
#[test]
fn the_ladder_resets_only_after_a_minute_of_healthy_ready_time() {
    let secs = std::time::Duration::from_secs;
    assert!(!super::ladder_resets(None), "not ready: nothing to reset");
    assert!(!super::ladder_resets(Some(secs(0))), "just became ready");
    assert!(
        !super::ladder_resets(Some(secs(15))),
        "a link that dies after 15 s keeps climbing"
    );
    assert!(!super::ladder_resets(Some(secs(59))));
    assert!(super::ladder_resets(Some(secs(60))));
    assert_eq!(super::HEALTHY_AFTER, secs(60));
    assert_eq!(
        super::unavailable_words("Pixel 9"),
        "Can't reach Pixel 9 · Try again"
    );
}

/// Any sign of the phone starts an exhausted ladder over, clears the
/// "can't reach" state, and goes for the connection at once.
#[test]
fn a_sign_of_the_phone_restarts_the_ladder_and_clears_the_unavailable_state() {
    use crate::device_links::core::{DeviceManager, SessionLink};
    let dir = std::env::temp_dir().join(format!("desklink-sign-{}", uuid::Uuid::new_v4()));
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    // A remote id below ours: this side only asks the phone to restart,
    // so the test builds no peer.
    let binding = sessions
        .register_link("0".to_string(), SessionLink::test_placeholder("0"), true)
        .unwrap()
        .binding;
    let devices: Arc<Mutex<HashMap<String, crate::device_links::device::DeviceView>>> =
        Arc::default();
    let info = crate::device_links::device_info::DeviceInfo {
        id: "0".into(),
        name: "Pixel 9".into(),
        device_type: "phone".into(),
        protocol_version: 9,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    let mut card =
        crate::device_links::device::DeviceView::from_info(&info, "10.0.0.2".into(), true);
    card.last_error = Some(super::unavailable_words("Pixel 9"));
    devices.lock().unwrap().insert("0".into(), card);
    let exhausted = RECOVERY_BACKOFF_MILLIS.len() as u32;
    sessions
        .with_session(&binding, |session| {
            session.webrtc_recovery_attempt = exhausted
        })
        .unwrap();
    let events = Arc::default();
    super::recovery_sign("0", &sessions, &config, &devices, &events);
    assert_eq!(
        sessions
            .with_session(&binding, |s| s.webrtc_recovery_attempt)
            .unwrap(),
        0
    );
    assert_eq!(devices.lock().unwrap()["0"].last_error, None);
    // A phone with no session is no sign of anything.
    super::recovery_sign("unknown", &sessions, &config, &devices, &events);
    let _ = std::fs::remove_dir_all(dir);
}

fn paired_session(
    sessions: &crate::device_links::core::DeviceManager,
    id: &str,
) -> crate::device_links::core::SessionBinding {
    sessions
        .register_link(
            id.to_string(),
            crate::device_links::core::SessionLink::test_placeholder(id),
            true,
        )
        .unwrap()
        .binding
}

/// Review finding G: a work thread that did not start is an error, not a
/// sender whose jobs never run.
#[test]
fn a_work_lane_that_cannot_start_is_an_error() {
    let failed = super::spawn_work_lane_with(|_, _| Err(std::io::Error::other("no threads left")));
    assert!(failed.unwrap_err().contains("no threads left"));
    assert!(super::spawn_work_lane().is_ok());
}

/// Review finding D: "Try Again" resets an exhausted ladder, clears the
/// "Can't reach" state and wakes the phone's backed-off addresses.
#[test]
fn try_again_restarts_the_ladder_clears_the_state_and_wakes_the_addresses() {
    let dir = std::env::temp_dir().join(format!("desklink-try-{}", uuid::Uuid::new_v4()));
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(crate::device_links::core::DeviceManager::new());
    // "00" sorts below any device id: this side only asks the phone to
    // restart, so the test builds no peer.
    let binding = paired_session(&sessions, "00");
    let devices: Arc<Mutex<HashMap<String, crate::device_links::device::DeviceView>>> =
        Arc::default();
    let info = crate::device_links::device_info::DeviceInfo {
        id: "00".into(),
        name: "Pixel 9".into(),
        device_type: "phone".into(),
        protocol_version: 9,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    let mut card =
        crate::device_links::device::DeviceView::from_info(&info, "10.0.0.2".into(), true);
    card.last_error = Some(super::unavailable_words("Pixel 9"));
    assert!(super::is_unavailable(card.last_error.as_deref().unwrap()));
    devices.lock().unwrap().insert("00".into(), card);
    sessions
        .with_session(&binding, |session| {
            session.webrtc_recovery_attempt = RECOVERY_BACKOFF_MILLIS.len() as u32
        })
        .unwrap();
    let ip: std::net::IpAddr = "10.77.0.2".parse().unwrap();
    crate::device_links::daemon::discovery::test_dead_address("00", ip);
    assert!(crate::device_links::daemon::discovery::test_due("00").is_empty());
    let events = Arc::default();
    super::reconnect("00", &sessions, &config, &devices, &events);
    assert_eq!(
        sessions
            .with_session(&binding, |s| s.webrtc_recovery_attempt)
            .unwrap(),
        0
    );
    assert_eq!(devices.lock().unwrap()["00"].last_error, None);
    assert_eq!(
        crate::device_links::daemon::discovery::test_due("00"),
        [ip],
        "its addresses are tried again at once"
    );
    crate::device_links::daemon::discovery::forget_addresses("00");
    let _ = std::fs::remove_dir_all(dir);
}

/// The pre-W2 rollout: an app without the liveness channel can never become
/// feature-ready; after two peers like that the words say what to do.
#[test]
fn a_phone_without_the_liveness_channel_is_told_to_update() {
    assert!(super::lacks_liveness_channel(true, true, false));
    assert!(!super::lacks_liveness_channel(true, true, true));
    assert!(
        !super::lacks_liveness_channel(false, false, false),
        "a network that never connected says nothing about the app"
    );
    let id = "old-app-phone";
    assert_eq!(
        super::exhausted_words(id, "Pixel 9"),
        "Can't reach Pixel 9 · Try again"
    );
    super::note_lacking(id, true);
    assert!(super::exhausted_words(id, "Pixel 9").starts_with("Can't reach"));
    super::note_lacking(id, true);
    assert_eq!(
        super::exhausted_words(id, "Pixel 9"),
        "Update DeskLink on Pixel 9"
    );
    assert!(!super::is_unavailable(&super::exhausted_words(
        id, "Pixel 9"
    )));
    // One good connection forgets it.
    super::note_lacking(id, false);
    assert!(super::is_unavailable(&super::exhausted_words(
        id, "Pixel 9"
    )));
}

/// Review finding E: several signs within a second are one.
#[test]
fn signs_of_a_phone_within_a_second_count_once() {
    let start = std::time::Instant::now();
    let at = |ms| start + std::time::Duration::from_millis(ms);
    assert!(super::sign_is_new("sign-debounce-phone", at(0)));
    assert!(!super::sign_is_new("sign-debounce-phone", at(10)));
    assert!(!super::sign_is_new("sign-debounce-phone", at(999)));
    assert!(super::sign_is_new("sign-debounce-phone", at(1000)));
    assert!(super::sign_is_new("another-phone", at(5)));
}

/// Review finding E: one start at a time per phone.
#[test]
fn only_one_start_per_phone_runs_at_a_time() {
    let first = super::claim_start("claim-phone").expect("free");
    assert!(super::claim_start("claim-phone").is_none(), "taken");
    assert!(super::claim_start("another-claim-phone").is_some());
    drop(first);
    assert!(super::claim_start("claim-phone").is_some(), "released");
}

/// Review finding E: the install itself refuses a second peer, so two
/// starters that both saw "no peer" cannot replace one another.
#[test]
fn installing_a_peer_if_none_refuses_a_second_one() {
    use crate::device_links::core::{DeviceManager, Installed};
    use crate::device_links::webrtc::ice::IceServerConfig;
    use crate::device_links::webrtc::peer_connection::DesktopWebRtcPeer;
    use crate::device_links::webrtc::transfer_manager::WebRtcTransferManager;
    let dir = std::env::temp_dir().join(format!("desklink-install-{}", uuid::Uuid::new_v4()));
    let sessions = DeviceManager::new();
    let binding = paired_session(&sessions, "install-phone");
    let manager =
        || Arc::new(WebRtcTransferManager::new(dir.join("state"), dir.join("downloads")).unwrap());
    let peer =
        || DesktopWebRtcPeer::new(true, mpsc::channel().0, &IceServerConfig::default()).unwrap();
    let wire =
        WebRtcWireBinding::from_attempt("a", "b", &uuid::Uuid::new_v4().to_string()).unwrap();
    let first = sessions
        .install_webrtc_peer_if_none(
            &binding,
            "attempt-1".into(),
            wire.clone(),
            peer(),
            manager(),
        )
        .unwrap();
    assert!(matches!(first, Installed::Replaced(None)));
    match sessions
        .install_webrtc_peer_if_none(&binding, "attempt-2".into(), wire, peer(), manager())
        .unwrap()
    {
        Installed::Occupied(unused) => unused.close(),
        Installed::Replaced(_) => panic!("the second install replaced the first"),
    }
    assert_eq!(
        sessions.active_webrtc_attempt(&binding).unwrap().as_deref(),
        Some("attempt-1"),
        "the first peer stays"
    );
    if let Ok(Some(peer)) = sessions.clear_webrtc_if_current(&binding, "attempt-1", "test".into()) {
        peer.close();
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// File and terminal messages run on their own thread, in order: a slow
/// one never holds up the thread that serves liveness.
#[test]
fn a_slow_file_message_does_not_hold_up_the_worker_and_runs_in_order() {
    let lane = super::spawn_work_lane().unwrap();
    let (done_tx, done) = mpsc::channel();
    let slow = done_tx.clone();
    let started = std::time::Instant::now();
    lane.send(Box::new(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        slow.send(1).unwrap();
    }))
    .unwrap();
    lane.send(Box::new(move || done_tx.send(2).unwrap()))
        .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "handing over is instant"
    );
    assert_eq!(
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        1
    );
    assert_eq!(
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        2,
        "in order"
    );
    // A panicking job does not end the lane.
    let (after_tx, after) = mpsc::channel();
    lane.send(Box::new(|| panic!("boom"))).unwrap();
    lane.send(Box::new(move || after_tx.send(()).unwrap()))
        .unwrap();
    after
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
}

#[test]
fn waking_restarts_the_silence_clock_instead_of_judging_the_doze() {
    let secs = std::time::Duration::from_secs;
    let start = std::time::Instant::now();
    let last_heard = start;
    let now = start + secs(34);
    // Not woke: the clock keeps running.
    assert_eq!(super::clock_after_wake(false, last_heard, now), last_heard);
    // Woke after 34 s of doze: silence starts from now, so the 15 s
    // dead time is judged afresh and one probe cycle fits first.
    let restarted = super::clock_after_wake(true, last_heard, now);
    assert_eq!(restarted, now);
    let silence = now + secs(1) - restarted;
    assert_eq!(super::liveness(silence, secs(1), false), Liveness::Fine);
    assert_eq!(super::liveness(secs(5), secs(5), false), Liveness::Probe);
    assert_eq!(
        super::liveness(now + secs(15) - restarted, secs(5), false),
        Liveness::Dead,
        "a phone that never answers is still declared gone"
    );
    // Waking is reported once, by the heartbeat that says so.
    let device = "wake-clock-phone";
    let mut asleep = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    asleep.set("dozing", true);
    assert!(!note_heartbeat(device, &asleep));
    let awake = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    assert!(note_heartbeat(device, &awake));
    assert!(!note_heartbeat(device, &awake));
}

/// Both sides give up in the same second: the request that arrives for an
/// attempt this side already replaced is ignored, so one rebuild happens.
#[test]
fn simultaneous_give_ups_cause_one_rebuild_and_only_the_initiator_rebuilds() {
    use super::{restart_request_action as action, RestartAction::*};
    assert_eq!(
        action(true, Some("old"), "old"),
        Restart,
        "the attempt it is about"
    );
    assert_eq!(action(true, None, "old"), Restart, "nothing rebuilt yet");
    assert_eq!(action(true, Some("new"), "old"), Ignore, "already rebuilt");
    assert_eq!(
        action(false, Some("old"), "old"),
        Ignore,
        "the responder never offers"
    );
    // Simultaneous offers: only the lower id offers; a higher one refuses.
    assert!(crate::device_links::local_goes_first("a", "b"));
    assert!(!crate::device_links::local_goes_first("b", "a"));
    assert!(!crate::device_links::local_goes_first("a", "a"));
}

#[test]
fn a_resume_or_network_change_probes_the_peer_at_once() {
    let id = "probe-soon-phone";
    assert!(!super::take_probe_now(id));
    super::probe_soon(id);
    assert!(super::take_probe_now(id), "probed on the next pass");
    assert!(!super::take_probe_now(id), "once");
}

#[test]
fn a_dozing_phone_is_judged_by_its_most_recent_channel_and_probed_on_waking() {
    use std::time::Duration;
    let s = Duration::from_secs;
    assert_eq!(
        effective_silence(s(30), Some(s(4)), true),
        s(4),
        "LAN answered"
    );
    assert_eq!(
        effective_silence(s(30), Some(s(4)), false),
        s(30),
        "awake: data only"
    );
    assert_eq!(effective_silence(s(30), None, true), s(30));

    let device = "waking-test-phone";
    let mut heartbeat = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    heartbeat.set("dozing", true);
    note_heartbeat(device, &heartbeat);
    assert!(!take_probe_now(device), "falling asleep probes nothing");
    heartbeat.set("dozing", false);
    note_heartbeat(device, &heartbeat);
    assert!(take_probe_now(device), "waking probes at once");
    assert!(!take_probe_now(device), "once");
    note_heartbeat(device, &heartbeat);
    assert!(!take_probe_now(device), "already awake: nothing new");
}

#[test]
fn dozing_is_read_from_the_heartbeat_and_cleared_by_one_without_it() {
    let mut heartbeat = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    assert!(!reports_dozing(&heartbeat), "older peers send no body");
    heartbeat.set("dozing", "yes");
    assert!(!reports_dozing(&heartbeat), "not a boolean");
    heartbeat.set("dozing", true);
    assert!(reports_dozing(&heartbeat));

    let device = "dozing-test-phone";
    note_heartbeat(device, &heartbeat);
    assert!(is_dozing(device));
    note_heartbeat(
        device,
        &NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT),
    );
    assert!(!is_dozing(device), "a heartbeat without it is awake");
    heartbeat.set("dozing", true);
    note_heartbeat(device, &heartbeat);
    heartbeat.set("dozing", false);
    note_heartbeat(device, &heartbeat);
    assert!(!is_dozing(device));
}

#[test]
fn a_fresh_offer_replaces_the_current_attempt() {
    assert_eq!(
        offer_attempt_disposition(Some("current"), "current").unwrap(),
        OfferAttemptDisposition::ReuseCurrent
    );
    // A fresh signed offer replaces the peer: the initiator restarted.
    assert_eq!(
        offer_attempt_disposition(Some("current"), "fresh").unwrap(),
        OfferAttemptDisposition::InstallNew
    );
}

/// A share worker that is not reading takes at most [`PORTAL_QUEUE`]
/// commands; the rest of an input flood is dropped, not queued without
/// bound (and later replayed as stale clicks).
#[test]
fn a_stalled_share_worker_does_not_build_a_backlog() {
    let (sender, receiver) = mpsc::sync_channel::<super::DesktopPortalCommand>(super::PORTAL_QUEUE);
    let mut taken = 0;
    for _ in 0..super::PORTAL_QUEUE * 4 {
        if sender
            .try_send(super::DesktopPortalCommand::Input(Vec::new()))
            .is_ok()
        {
            taken += 1;
        }
    }
    assert_eq!(taken, super::PORTAL_QUEUE);
    drop(receiver);
    // And the per-phone rate: more than a second's worth in one second is
    // refused.
    let key = "rate-test-phone";
    let allowed = (0..super::PORTAL_INPUT_PER_SECOND + 500)
        .filter(|_| super::PORTAL_INPUT_RATE.allow(key))
        .count();
    assert_eq!(allowed, super::PORTAL_INPUT_PER_SECOND as usize);
}

/// A phone asking for a second display of 8192 x 8192 is refused; real
/// phone and tablet sizes are not.
#[test]
fn an_extended_display_has_a_sane_size() {
    assert!(super::extend_size_ok(1080, 2400));
    assert!(super::extend_size_ok(2560, 1600));
    assert!(super::extend_size_ok(3840, 2160));
    assert!(!super::extend_size_ok(8192, 8192));
    assert!(!super::extend_size_ok(4096, 4096));
    assert!(!super::extend_size_ok(100, 2000));
    assert!(!super::extend_size_ok(0, 0));
    assert!(!super::extend_size_ok(-5, 1080));
}

/// A lane needs the capability on both sides, a config to pin with, and an
/// ICE pair that is the local network: without any one, no lane target.
#[test]
fn a_link_offers_a_lane_only_with_the_capability_a_config_and_a_local_pair() {
    use crate::device_links::features::{forget_peer_profile, record_peer_profile};
    use crate::device_links::packet::PACKET_TYPE_FILE_LANE;
    use crate::device_links::webrtc::transfer_manager::FileLink;
    let dir = std::env::temp_dir().join(format!("desklink-lane-link-{}", uuid::Uuid::new_v4()));
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let peer = format!("lane-peer-{}", uuid::Uuid::new_v4());
    let link = |config: Option<Arc<Mutex<Config>>>| super::PeerFileLink {
        peer: std::sync::Weak::new(),
        wire: WebRtcWireBinding {
            sender_device_id: "desk".into(),
            peer_device_id: peer.clone(),
            session_id: 1,
            generation: 1,
        },
        config,
        lane_seen: Arc::new(Mutex::new(None)),
        refreshing: Arc::new(AtomicBool::new(false)),
    };
    let with = |marker: bool| {
        let mut types = vec!["desklink.heartbeat".to_string()];
        if marker {
            types.push(PACKET_TYPE_FILE_LANE.to_string());
        }
        record_peer_profile(&peer, &types, &types);
    };
    // The peer's handover lacks the marker: no lane, whatever the path.
    with(false);
    assert!(link(Some(config.clone())).lane_target().is_none());
    // It has it, but there is no ICE pair to be local: still none.
    with(true);
    assert!(link(Some(config.clone())).lane_target().is_none());
    assert!(!link(Some(config.clone())).relayed());
    // And with no config to pin with, none either.
    assert!(link(None).lane_target().is_none());
    forget_peer_profile(&peer);
    let _ = std::fs::remove_dir_all(dir);
}

/// The path is read once, waiting; after that an old reading answers at
/// once and is refreshed behind the caller's back.
#[test]
fn a_stale_path_reading_answers_at_once_and_is_refreshed_in_the_background() {
    let link = super::PeerFileLink {
        peer: std::sync::Weak::new(),
        wire: WebRtcWireBinding {
            sender_device_id: "desk".into(),
            peer_device_id: "phone".into(),
            session_id: 1,
            generation: 1,
        },
        config: None,
        lane_seen: Arc::new(Mutex::new(None)),
        refreshing: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(link.path(), (None, false), "the first look reads");
    // An old reading that says relayed, with a pair address.
    let old = (Some(std::net::IpAddr::from([192, 168, 1, 3])), true);
    *link.lane_seen.lock().unwrap() =
        Some((std::time::Instant::now() - Duration::from_secs(30), old));
    assert_eq!(link.path(), old, "the old reading, without waiting");
    let deadline = Instant::now() + Duration::from_secs(5);
    while link.path() == old {
        assert!(Instant::now() < deadline, "never refreshed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(link.path(), (None, false), "the refresh replaced it");
}

/// No GStreamer state change runs under the remote-session lock: showing a
/// parked phone stream again starts a GTK sink, which waits for the main
/// loop, and the main loop's Phone Screen tick takes that lock every 33 ms.
/// It did so at a reshare (p3.4): the desk hung for good, the stalled
/// receive path took the data channels down with it.
#[test]
fn a_parked_phone_stream_is_shown_again_outside_the_session_lock() {
    let source = crate::source_scan::production(include_str!("envelopes.rs"));
    let start = source
        .find("let request_control = {")
        .expect("the session-lock block");
    let end = start + source[start..].find("\n        };").expect("its end");
    let locked = &source[start..end];
    assert!(locked.contains("remote_session::sessions()"), "{locked}");
    for call in [
        "resume_parked(",
        "clear_stream(",
        "stop_desktop_portal_session(",
    ] {
        assert!(!locked.contains(call), "{call} under the session lock");
    }
    assert!(
        source[end..].contains("resume_parked("),
        "still shown again"
    );
}

use super::{apply_input_batch, DesktopSession, PortalInputAction};

/// A desktop whose pointer moves fail (a D-Bus timeout, "Invalid position").
struct RefusingMoves {
    applied: std::cell::RefCell<Vec<String>>,
}

impl DesktopSession for RefusingMoves {
    fn active(&self) -> crate::device_links::webrtc::remote_desktop_portal::PortalStream {
        crate::device_links::webrtc::remote_desktop_portal::PortalStream {
            node_id: 1,
            width: 100,
            height: 100,
            pipewire_serial: None,
        }
    }
    fn stream_count(&self) -> usize {
        1
    }
    fn next_stream(&self) -> crate::device_links::webrtc::remote_desktop_portal::PortalStream {
        self.active()
    }
    fn knows_active_pixels(&self) -> bool {
        true
    }
    fn set_active_pixels(&self, _size: (i32, i32)) {}
    fn is_session_closed(&self) -> bool {
        false
    }
    fn pointer_motion(&self, _dx: f64, _dy: f64) -> Result<(), String> {
        Err("timed out".into())
    }
    fn pointer_motion_absolute(&self, _x: f64, _y: f64) -> Result<(), String> {
        Err("Invalid position".into())
    }
    fn pointer_button(&self, evdev_button: i32, pressed: bool) -> Result<(), String> {
        self.applied
            .borrow_mut()
            .push(format!("button {evdev_button} {pressed}"));
        Ok(())
    }
    fn pointer_axis(&self, _dx: f64, _dy: f64, _finish: bool) -> Result<(), String> {
        Ok(())
    }
    fn keyboard_keysym(&self, _keysym: i32, _pressed: bool) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn a_refused_move_still_lets_the_button_go() {
    let desk = RefusingMoves {
        applied: Default::default(),
    };
    let mut held = std::collections::HashSet::new();
    let press = PortalInputAction::Button {
        evdev_code: 0x110,
        pressed: true,
    };
    assert!(!apply_input_batch(&desk, vec![press], &mut held, || {}));
    assert!(held.contains(&(false, 0x110)));
    let batch = vec![
        PortalInputAction::AbsoluteMotion(10.0, 10.0),
        PortalInputAction::Button {
            evdev_code: 0x111,
            pressed: true,
        },
        PortalInputAction::Button {
            evdev_code: 0x110,
            pressed: false,
        },
    ];
    assert!(!apply_input_batch(&desk, batch, &mut held, || {}));
    assert!(
        held.is_empty(),
        "the release after the failed move was applied"
    );
    assert_eq!(
        *desk.applied.borrow(),
        vec!["button 272 true", "button 272 false"],
        "a press after the failure is skipped, the release is not"
    );
}

/// A release never falls to the rate limit or a full queue; the rest may.
#[test]
fn only_releases_survive_a_rate_limited_or_full_input_batch() {
    let batch = vec![
        PortalInputAction::AbsoluteMotion(1.0, 1.0),
        PortalInputAction::Button {
            evdev_code: 0x110,
            pressed: true,
        },
        PortalInputAction::Button {
            evdev_code: 0x110,
            pressed: false,
        },
        PortalInputAction::Key {
            keysym: 0x61,
            pressed: false,
        },
    ];
    assert_eq!(
        super::desktop_input::releases(&batch),
        vec![
            PortalInputAction::Button {
                evdev_code: 0x110,
                pressed: false
            },
            PortalInputAction::Key {
                keysym: 0x61,
                pressed: false
            },
        ]
    );
}

/// After a failure, a release goes only for what is held here: a release
/// of a button this side never pressed is not sent.
#[test]
fn after_a_failure_only_held_inputs_are_released() {
    let desk = RefusingMoves {
        applied: Default::default(),
    };
    let mut held = std::collections::HashSet::new();
    let batch = vec![
        PortalInputAction::AbsoluteMotion(10.0, 10.0),
        PortalInputAction::Button {
            evdev_code: 0x112,
            pressed: false,
        },
    ];
    assert!(!apply_input_batch(&desk, batch, &mut held, || {}));
    assert!(
        desk.applied.borrow().is_empty(),
        "{:?}",
        desk.applied.borrow()
    );
}
