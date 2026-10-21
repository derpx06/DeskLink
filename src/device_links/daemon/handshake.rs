use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::handle::DaemonEvent;
use super::network::read_ssl_packet;
use super::packet_handler::packet_read_loop;
use super::state::{mark_status, upsert_device};
use super::validation::{
    enforce_unpaired_link_limit, validate_certificate_device_id, validate_pinned_certificate,
};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionBinding, SessionLink};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::PACKET_TYPE_WEBRTC_SIGNAL_V1;

/// Idle read timeout on an established device link. It also bounds how long a
/// send can wait for the stream mutex the reader holds — signalling (SDP and
/// ICE during a rebuild) included, so it is short. ~40 idle wakeups a
/// second per link.
pub(super) const READ_TIMEOUT: Duration = Duration::from_millis(20);

pub(super) fn finish_secure_link(
    initial_info: DeviceInfo,
    mut stream: SslStream<TcpStream>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    let identity = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(0);
    stream
        .write_all(&identity.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;

    let secure_packet = read_ssl_packet(&mut stream)?;
    let secure_info = DeviceInfo::from_identity_packet(&secure_packet)
        .ok_or_else(|| "Invalid secure identity".to_string())?;
    if secure_info.id != initial_info.id
        || secure_info.protocol_version != initial_info.protocol_version
    {
        return Err("Secure identity changed during handshake".to_string());
    }

    let cert = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| "Remote peer did not provide a certificate".to_string())?;
    validate_certificate_device_id(&cert, &secure_info.id)?;
    let cert_pem = String::from_utf8(cert.to_pem().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    let remote_public_der = cert
        .public_key()
        .and_then(|key| key.public_key_to_der())
        .map_err(|err| err.to_string())?;
    let local_public_der = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .key()
        .public_key_to_der()
        .map_err(|err| err.to_string())?;
    let pinned = validate_pinned_certificate(&config, &secure_info.id, &cert);
    let new_certificate = certificate_admission(
        pinned,
        crate::device_links::pairing_qr::current_uri().is_some(),
    )?;
    enforce_unpaired_link_limit(&config, &sessions, &secure_info.id)?;

    // A link presenting a new certificate is never paired, whatever the trust
    // store says about its device id; only a valid QR proof (or a pairing the
    // user accepts) can make it so, and only then is its certificate stored.
    let paired = !new_certificate
        && config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?
            .is_trusted(&secure_info.id);
    if paired && crate::device_links::relay_link::client::force_relay() {
        return Err("LAN links are disabled for testing (DESKLINK_FORCE_RELAY)".to_string());
    }
    // Merge, never replace. `insert` here rebuilt the `DeviceView` from the
    // identity packet, which zeroes notifications, battery, volume, media,
    // and commands — so a Wi-Fi roam blanked every summary chip.
    // Worse, the emptied notification set made the UI's dedup table prune
    // itself, and the phone's next resync re-delivered every already-seen
    // notification as a fresh desktop popup.
    //
    // Status is deliberately not set: `reconcile_connection_status` owns
    // reachability, and asserting it from a handshake path is what previously
    // left dead devices reading as connected.
    upsert_device(
        &devices,
        &secure_info,
        stream
            .get_ref()
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        paired,
    );

    // Keep the TLS stream in one mode for its entire lifetime.  Toggling an
    // OpenSSL stream between nonblocking and blocking mode from the reader
    // and sender threads races with OpenSSL's internal WANT_READ/WANT_WRITE
    // state and can surface a false "nonblocking read call would have
    // blocked" error during WebRTC signaling.  A short read timeout keeps
    // shutdown responsive without changing the socket mode underneath the
    // active TLS stream.
    //
    // The timeout also bounds write latency. The reader holds the stream mutex
    // across each blocking read, so a feature send waits for at most one
    // timeout before it can acquire the lock. At 250ms that was longer than a
    // frame of remote-input motion; 50ms keeps the pointer responsive while
    // still leaving the reader mostly parked.
    stream
        .get_ref()
        .set_nonblocking(false)
        .map_err(|err| err.to_string())?;
    stream
        .get_ref()
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|err| err.to_string())?;
    stream
        .get_ref()
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    let transport: Arc<dyn crate::device_links::core::LinkTransport> = Arc::new(
        super::network::TlsLinkTransport::new(Arc::new(Mutex::new(stream))),
    );
    let link = SessionLink::new(
        Arc::clone(&transport),
        cert_pem,
        local_public_der,
        remote_public_der,
        secure_info,
    );
    // A relay link is standby capacity: a working LAN socket always replaces
    // it, even while it is live and answering.
    let supersede_live = sessions.bootstrap_is_relay(&initial_info.id);
    attach_bootstrap_link(
        &initial_info.id,
        link,
        transport,
        paired,
        supersede_live,
        config,
        devices,
        sessions,
        events,
    )
}

/// Whether a link may be admitted given the pinned-certificate check: as
/// itself (`Ok(false)`), or — a certificate that differs from the stored
/// one, while a QR code is on screen — as an *unpaired* newcomer
/// (`Ok(true)`), so a reinstalled phone can re-pair by scanning. Otherwise
/// the mismatch refuses the link.
pub(super) fn certificate_admission(
    pinned: Result<(), String>,
    qr_live: bool,
) -> Result<bool, String> {
    match pinned {
        Ok(()) => Ok(false),
        Err(_) if qr_live => {
            debug_log!("[Daemon] Certificate changed; admitted unpaired while a QR code is shown");
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// Installs an authenticated bootstrap link — a LAN TLS socket or a relay
/// link — as `device_id`'s signaling channel, starting its reader and, for a
/// paired WebRTC peer, the negotiation.
///
/// `supersede_live` skips the rule that keeps a live, answering incumbent:
/// set when the incumbent is a relay link and a LAN socket arrives, and when
/// a relay link must take over because the peer is signaling through it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn attach_bootstrap_link(
    device_id: &str,
    link: SessionLink,
    transport: Arc<dyn crate::device_links::core::LinkTransport>,
    paired: bool,
    supersede_live: bool,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    // Android may refresh the short-lived LAN bootstrap socket while its
    // authenticated WebRTC peer is healthy.  That socket must not become a
    // second session: discarding it preserves the existing generation and
    // avoids tearing down all paired features for a discovery refresh.
    //
    // **But only while the retained bootstrap link is still open.** Once the
    // phone has closed the old socket, keeping it and discarding the fresh one
    // leaves this side with no signalling channel at all — and the moment the
    // WebRTC peer fails, the restart request it needs to send has nowhere to
    // go. That produced a device stuck on "Connecting" indefinitely: the
    // handover had completed, the peer had since failed, and every
    // `RestartRequest` died with "Session link is closed" against a socket the
    // peer had already replaced.
    // A retained link must also have been *heard from*. `has_live_bootstrap_link`
    // reads `is_closed()`, a flag that only flips when this side's own reader
    // sees EOF — so a peer that vanished without a FIN leaves it `false`
    // forever, and this guard then refuses every replacement the peer offers.
    //
    // That is the livelock, and it has been observed in both directions: once
    // with the desktop discarding 170 fresh sockets while holding a dead one,
    // and once with the phone refusing to dial a desktop idling in wait. The
    // newcomer wins against a silent incumbent, which is the liveness override
    // Syncthing still lacks (issue #9337) and the reason a zombie cannot block
    // its own replacement here.
    let incumbent_is_answering = sessions
        .silence(device_id)
        .is_some_and(|silence| silence < super::HEARTBEAT_IDLE * 2);
    // A new restart epoch means the peer's previous process — and with it the
    // WebRTC peer this session holds — is gone. Neither keep the old socket
    // nor adopt into the old session: register, which clears the dead peer.
    let restarted = sessions.peer_restarted(device_id, link.info.instance_id.as_deref());
    if restarted {
        debug_log!(
            "[DL-WRTC-BOOT] {} restarted (new instance); replacing its session",
            device_id
        );
    }
    if !restarted
        && !supersede_live
        && sessions.has_ready_webrtc(device_id)
        && sessions.has_live_bootstrap_link(device_id)
        && incumbent_is_answering
    {
        debug_log!(
            "[DL-WRTC-BOOT] ignoring refreshed bootstrap socket for {}; retaining feature-ready WebRTC session",
            device_id
        );
        link.close();
        return Ok(());
    }

    // The incumbent is closed, or open but silent (a frozen phone's old socket
    // while its new one arrives), and its WebRTC feature transport is still
    // healthy. Adopt the replacement socket into the existing session
    // rather than registering a new one: `register_link` bumps the generation
    // and clears the peer and handover unconditionally, so re-registering here
    // destroyed a *working* transport every time the phone refreshed its
    // bootstrap socket -- and the handover that followed dropped its own
    // bootstrap socket, which brought the phone straight back here. That loop
    // is what pinned the UI on "Connecting" forever while battery and
    // notifications kept arriving over the link underneath it.
    //
    // Android already treats a replacement LAN connection as signaling
    // capacity only (`onConnectionReceived`'s retained binding); this is the
    // desktop half of the same rule.
    let adopted = if restarted || supersede_live {
        None
    } else {
        sessions.adopt_bootstrap_link(device_id, &transport)
    };
    if let Some((binding, replaced)) = adopted {
        // Outside the session lock: the old socket's reader wakes, sees the
        // link carries a newer socket, and leaves quietly.
        replaced.shutdown();
        debug_log!(
            "[DL-WRTC-BOOT] adopted a replacement bootstrap socket for {} (session {}, generation {}); kept the feature-ready WebRTC transport",
            binding.device_id, binding.session_id, binding.generation
        );
        let read_devices = Arc::clone(&devices);
        let read_sessions = Arc::clone(&sessions);
        let read_config = Arc::clone(&config);
        let read_events = Arc::clone(&events);
        thread::spawn(move || {
            packet_read_loop(
                binding,
                read_devices,
                read_sessions,
                read_config,
                read_events,
            )
        });
        return Ok(());
    }

    let registration = sessions
        .register_link(device_id.to_string(), link, paired)
        .map_err(|error| error.to_string())?;
    if let Some(replaced) = registration.replaced_link {
        replaced.close();
    }
    if let Some(replaced) = registration.replaced_webrtc {
        replaced.close();
    }

    let binding = registration.binding;
    debug_log!(
        "[DL-WRTC-BOOT] secure bootstrap link ready for {} (session {}, generation {})",
        binding.device_id,
        binding.session_id,
        binding.generation
    );
    let remote_supports_webrtc = paired
        && binding
            .link
            .info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1);
    debug_log!(
        "[DL-WRTC-BOOT] peer WebRTC signaling capability: {}",
        remote_supports_webrtc
    );
    let read_devices = Arc::clone(&devices);
    let read_sessions = Arc::clone(&sessions);
    let read_config = Arc::clone(&config);
    let read_binding = binding.clone();
    let read_events = Arc::clone(&events);
    thread::spawn(move || {
        packet_read_loop(
            read_binding,
            read_devices,
            read_sessions,
            read_config,
            read_events,
        )
    });

    if remote_supports_webrtc {
        schedule_webrtc_bootstrap(
            binding,
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
            Arc::clone(&events),
        );
    }
    Ok(())
}

/// Start SDP/ICE only after the normal packet reader owns the TLS stream.
/// Both implementations use buffered identity readers during the secure
/// handshake; signaling sent before those readers are retired can otherwise
/// be prefetched and lost. A bounded retry is safe because each request is
/// signed and replay-protected, and it stops as soon as an attempt is active.
fn schedule_webrtc_bootstrap(
    binding: SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) {
    // When the phone is the initiator (its id sorts lower) it offers on every
    // fresh link by itself, and each `RestartRequest` makes it discard the
    // offer it already has in flight — four of them cost a slow phone up to
    // four offers. So the responder asks once, only if nothing arrived in 3 s.
    let desktop_initiates = config
        .lock()
        .map(|config| config.local_device_info().id < binding.device_id)
        .unwrap_or(true);
    let delays: &'static [u64] = if desktop_initiates {
        &[250, 1_000, 2_000, 4_000]
    } else {
        &[3_000]
    };
    thread::spawn(move || {
        for &delay in delays {
            thread::sleep(Duration::from_millis(delay));
            if !sessions.is_current(&binding) {
                debug_log!(
                    "[DL-WRTC-BOOT] bootstrap scheduler stopped for stale generation {}",
                    binding.generation
                );
                return;
            }
            if sessions
                .active_webrtc_attempt(&binding)
                .ok()
                .flatten()
                .is_some()
            {
                return;
            }
            if let Err(error) = crate::device_links::webrtc::coordinator::start_initiator(
                binding.clone(),
                Arc::clone(&sessions),
                Arc::clone(&config),
                Arc::clone(&devices),
                Arc::clone(&events),
            ) {
                eprintln!("[Daemon] DeskLink WebRTC bootstrap attempt failed: {error}");
            }
        }
    });
}

pub(super) fn handle_disconnect(
    binding: &SessionBinding,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    reason: String,
) -> bool {
    let result = sessions.bootstrap_disconnected_if_current(binding, reason);
    if !result.was_current {
        return false;
    }
    if let Some(link) = result.link {
        link.close();
    }
    if result.feature_transport_retained {
        debug_log!(
            "[DL-WRTC-BOOT] bootstrap link closed for {}; retained authenticated WebRTC feature transport",
            binding.device_id
        );
        return false;
    }
    if let Some(peer) = result.webrtc {
        peer.close();
    }
    mark_status(devices, &binding.device_id, DeviceStatus::Unreachable);
    true
}

#[cfg(test)]
mod tests {
    use super::certificate_admission;

    /// A changed certificate is a stranger, never the paired phone: refused,
    /// or — only while a QR code is on screen — admitted unpaired, so the
    /// scan's proof can re-pair a reinstalled phone.
    #[test]
    fn a_changed_certificate_is_admitted_only_unpaired_and_only_during_a_qr_scan() {
        assert_eq!(certificate_admission(Ok(()), false), Ok(false));
        assert_eq!(
            certificate_admission(Ok(()), true),
            Ok(false),
            "matching: as itself"
        );
        let changed = || Err("Trusted device certificate changed".to_string());
        assert!(
            certificate_admission(changed(), false).is_err(),
            "no QR: refused"
        );
        assert_eq!(
            certificate_admission(changed(), true),
            Ok(true),
            "QR live: unpaired"
        );
    }
}
