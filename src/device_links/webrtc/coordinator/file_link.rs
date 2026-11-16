//! Files over a peer: starting and cancelling transfers, the per-peer
//! [`FileLink`] the transfer manager writes through (lane address included),
//! and making a peer's transfer manager.
//!
//! Sibling edges: none. Outside: `transfer_manager`, `file_lane`,
//! `file_protocol`, `capture`, `features::peer_sends`, `transfer_log`.

use super::*;

/// The manager of the current authenticated WebRTC peer, if features may use
/// it. There is no LAN fallback once feature-ready is active.
fn ready_transfer_manager<'a>(
    sessions: &DeviceManager,
    transport: &'a FeatureTransportSnapshot,
) -> Result<&'a Arc<WebRtcTransferManager>, String> {
    if !sessions.is_current(&transport.binding) || !transport.web_rtc_ready {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    transport
        .transfer_manager
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer manager is unavailable".to_string())
}

/// Starts a desktop-originated transfer through the current authenticated
/// WebRTC peer.
pub(crate) fn start_file_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
) -> Result<(), String> {
    ready_transfer_manager(sessions, transport)?
        .start_send(source)
        .map(|_| ())
}

/// Sends a synced file (F8 P4) through the current peer; `content_sha256` is
/// the manifest's hash of it, which the receiver matches against what it asked
/// for.
pub(crate) fn start_folder_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
    folder: crate::device_links::webrtc::file_protocol::FolderRef,
    content_sha256: &str,
) -> Result<(), String> {
    ready_transfer_manager(sessions, transport)?
        .start_send_folder(source, folder, content_sha256)
        .map(|_| ())
}

/// Sends `source` as a clipboard copy (C3) of type `mime`.
pub(crate) fn start_clipboard_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
    mime: &str,
) -> Result<(), String> {
    ready_transfer_manager(sessions, transport)?
        .start_send_as(source, Some(mime))
        .map(|_| ())
}

/// Ends a running file transfer (`why`: `code::USER`, or `code::REMOVED` for
/// a Bucket item removed while it moves) and tells the peer.
///
/// The transfer is over locally whether or not the peer is still listening: a
/// cancelled transfer is never resumed, so its state goes either way.
pub(crate) fn cancel_file_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    transfer_id: &str,
    why: &'static str,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    transport
        .transfer_manager
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer manager is unavailable".to_string())?
        .cancel_transfer(transfer_id, why)
}

/// A lane's address on the ICE pair, and whether the pair is relayed.
type LanePath = (Option<std::net::IpAddr>, bool);

/// One peer's file channels, for the transfer manager. Holds the peer weakly:
/// the peer owns the manager, not the other way round.
pub(super) struct PeerFileLink {
    pub(super) peer: std::sync::Weak<DesktopWebRtcPeer>,
    pub(super) wire: WebRtcWireBinding,
    /// For fast lanes (W18): the stored pairing certificates. `None`: no lane.
    pub(super) config: Option<Arc<Mutex<Config>>>,
    /// The last look at the ICE path (reading it asks webrtcbin for stats).
    pub(super) lane_seen: Arc<Mutex<Option<(std::time::Instant, LanePath)>>>,
    /// A refresh of `lane_seen` is running on its own thread.
    pub(super) refreshing: Arc<std::sync::atomic::AtomicBool>,
}

impl PeerFileLink {
    /// The ICE pair's address for a lane and whether it is relayed. Reading it
    /// asks webrtcbin for stats (up to two seconds if it is slow), and this is
    /// asked on the file thread: the first look waits, every later one answers
    /// from the last reading at once and refreshes it on a thread of its own
    /// when it is more than two seconds old.
    pub(super) fn path(&self) -> LanePath {
        let seen = self.lane_seen.lock().ok().and_then(|seen| *seen);
        match seen {
            Some((at, path)) => {
                if at.elapsed() >= std::time::Duration::from_secs(2) {
                    self.refresh_in_background();
                }
                path
            }
            None => {
                let path = self
                    .peer
                    .upgrade()
                    .map_or((None, false), |peer| peer.lane_address());
                if let Ok(mut seen) = self.lane_seen.lock() {
                    *seen = Some((std::time::Instant::now(), path));
                }
                path
            }
        }
    }

    fn refresh_in_background(&self) {
        if self
            .refreshing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let (peer, seen, refreshing) = (
            self.peer.clone(),
            Arc::clone(&self.lane_seen),
            Arc::clone(&self.refreshing),
        );
        let spawned = std::thread::Builder::new()
            .name("desklink-lane-path".into())
            .spawn({
                let refreshing = Arc::clone(&refreshing);
                move || {
                    let path = peer
                        .upgrade()
                        .map_or((None, false), |peer| peer.lane_address());
                    if let Ok(mut seen) = seen.lock() {
                        *seen = Some((std::time::Instant::now(), path));
                    }
                    refreshing.store(false, std::sync::atomic::Ordering::Release);
                }
            });
        if spawned.is_err() {
            refreshing.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

impl FileLink for PeerFileLink {
    fn send_control(&self, control: &FileControl) -> Result<(), String> {
        let peer = self
            .peer
            .upgrade()
            .ok_or_else(|| "The WebRTC peer has gone".to_string())?;
        let encoded = control.to_json()?;
        let envelope = WebRtcEnvelope::create(
            &self.wire,
            WebRtcChannel::FileControl,
            FILE_CONTROL_MESSAGE_TYPE,
            &encoded,
            now_millis(),
        )?;
        let text = String::from_utf8(envelope.to_json()?)
            .map_err(|_| "DeskLink WebRTC file envelope was not UTF-8".to_string())?;
        peer.send_text(WebRtcChannel::FileControl, &text)
    }

    fn send_frame(&self, frame: Vec<u8>) -> Result<(), String> {
        self.peer
            .upgrade()
            .ok_or_else(|| "The WebRTC peer has gone".to_string())?
            .send_file_frame(frame)
    }

    fn buffered(&self) -> u64 {
        self.peer.upgrade().map_or(u64::MAX, |peer| {
            peer.buffered_amount(WebRtcChannel::FileData)
        })
    }

    fn relayed(&self) -> bool {
        self.path().1
    }

    fn lane_target(&self) -> Option<crate::device_links::webrtc::file_lane::LaneTarget> {
        use crate::device_links::features::peer_sends;
        let config = self.config.clone()?;
        let marker = crate::device_links::packet::PACKET_TYPE_FILE_LANE;
        let peer_id = &self.wire.peer_device_id;
        // Both sides advertise it: the intersection of the handover lists.
        if !(peer_accepts(peer_id, marker) && peer_sends(peer_id, marker)) {
            return None;
        }
        let addr = self.path().0?;
        Some(crate::device_links::webrtc::file_lane::LaneTarget {
            addr,
            config,
            local_id: self.wire.sender_device_id.clone(),
            peer_id: peer_id.clone(),
        })
    }
}

/// Binds the peer's file channels to its transfer manager.
pub(super) fn attach_transfer_manager(
    manager: &Arc<WebRtcTransferManager>,
    peer: &Arc<DesktopWebRtcPeer>,
    wire: &WebRtcWireBinding,
    config: Option<Arc<Mutex<Config>>>,
) {
    manager.attach(
        wire.clone(),
        Arc::new(PeerFileLink {
            peer: Arc::downgrade(peer),
            wire: wire.clone(),
            config,
            lane_seen: Arc::new(Mutex::new(None)),
            refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
    );
    // A sender waiting for room in the channel is woken when it drains.
    // Weakly: the peer must never keep its manager (and the manager's thread)
    // alive, nor the other way round.
    let waking = Arc::downgrade(manager);
    peer.set_file_wake(move || {
        if let Some(manager) = waking.upgrade() {
            manager.wake();
        }
    });
}

pub(super) fn new_transfer_manager(
    config: &Arc<Mutex<Config>>,
) -> Result<Arc<WebRtcTransferManager>, String> {
    let paths = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?
        .transfer_state_paths();
    // Moving old state is file IO: not under the configuration lock.
    let transfer_state = paths.prepare();
    let downloads = crate::device_links::transfer_log::downloads_dir()
        .unwrap_or_else(|| transfer_state.join("downloads"));
    let manager = Arc::new(WebRtcTransferManager::new(transfer_state, downloads)?);
    // A photo or scan a desk asked for (F3) has arrived.
    manager.set_completion_hook(|completed| {
        debug_log!(
            "[DeskLink] WebRTC transfer {} completed at {}",
            completed.transfer_id,
            completed.path.display()
        );
        if let Some(capture_id) = completed
            .capture_id
            .as_deref()
            .filter(|_| completed.incoming)
        {
            crate::device_links::webrtc::capture::file_arrived(capture_id);
        }
    });
    Ok(manager)
}
