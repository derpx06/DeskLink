//! RTP and socket tuning on a `webrtcbin`: the jitter buffer, immediate RTCP
//! feedback, retransmission per media section, stats, the ICE socket's
//! receive buffer, and which media an SSRC belongs to.
//!
//! Sibling edges: none.

use super::*;

pub(super) const JITTER_BUFFER_PROPERTY: &str = "latency";

/// Milliseconds of jitter buffering, against a 200ms default.
///
/// Both peers are on the same LAN and the session is interactive — a mirror you
/// are clicking on — so smoothing for hundreds of milliseconds of reordering
/// buys nothing and costs everything. 40ms still absorbs ordinary WiFi jitter.
///
/// The sink renders without clock sync (`attach_incoming_video`): measured on
/// a phone mirror, clock sync let the display fall 3–5 s behind as the phone's
/// timestamps drifted, so this buffer is the only smoothing left.
pub(super) const JITTER_BUFFER_LATENCY_MS: u32 = 40;

/// Floor for the regular RTCP interval of each RTP session.
///
/// GStreamer follows RFC 4585 strictly: one *early* RTCP packet per regular
/// interval, everything else waits for the next regular report — and with the
/// default bandwidth-derived interval that wait was seconds. Measured on a
/// phone mirror: retransmission requests "not sent early, sending with next
/// regular RTCP", 95 of them expiring unanswered, and 3–5 s freezes because
/// the keyframe request queued behind them too. libwebrtc sends NACK/PLI
/// immediately; a 20 ms floor gets within one frame of that.
const RTCP_MIN_INTERVAL_NS: u64 = 20_000_000;
/// RTCP budget in bytes per second, so the bandwidth rule does not stretch the
/// interval back out. It is a ceiling, not traffic: at 8 kB/s compound reports
/// carrying feedback still came out at a 280 ms interval, with early NACKs
/// refused inside it. Real use stays a few percent of the video stream.
const RTCP_BYTES_PER_SECOND: f64 = 64_000.0;

/// Makes every RTP session `webrtcbin` creates send feedback promptly.
/// Sessions only exist after negotiation, so this hooks their first SSRC.
pub(super) fn enable_immediate_rtcp_feedback(webrtcbin: &gst::Element) {
    let Some(rtpbin) = webrtcbin
        .downcast_ref::<gst::Bin>()
        .and_then(|bin| bin.by_name("rtpbin"))
    else {
        debug_log!("[DeskLink] webrtcbin has no rtpbin; RTCP feedback keeps its default timing");
        return;
    };
    rtpbin.connect("on-new-ssrc", false, |values| {
        let rtpbin = values[0].get::<gst::Element>().ok()?;
        let session_id = values[1].get::<u32>().ok()?;
        let session = rtpbin.emit_by_name::<Option<gst::Element>>("get-session", &[&session_id])?;
        if session.has_property("rtcp-min-interval", None) {
            session.set_property("rtcp-min-interval", RTCP_MIN_INTERVAL_NS);
        }
        if session.has_property("rtcp-fraction", None) {
            session.set_property("rtcp-fraction", RTCP_BYTES_PER_SECOND);
        }
        None
    });
}

/// NACKs the phone's video, which `webrtcbin` left un-NACKed.
///
/// `webrtcbin` (1.28 and main, `jitter_buffer_set_retransmission`) sets a new
/// jitterbuffer's `do-retransmission` by walking *every* SSRC the remote
/// description listed in the bundle, applying each one's transceiver `do-nack`
/// without comparing SSRCs: the last listed wins. The phone lists video, then
/// audio, so its video took audio's `false`. Measured live: lost 489, nack 0,
/// every loss a PLI and a keyframe (a ~2 s freeze per burst). This handler runs
/// after `webrtcbin`'s and decides from the SSRC's own m-section instead.
///
/// It also turns off `rtx-next-seqnum`, for every jitterbuffer. With it on, the
/// jitterbuffer guesses when the *next* packet is due (one frame interval after
/// the last) and, when it is not there 40 ms later, NACKs it and declares it
/// lost. The phone sends frames only when the picture changes (a still screen is
/// five refinement frames 100 ms apart, then silence), so every pause was
/// reported as a lost packet the phone had never sent: the depayloader turned it
/// into a missing-data gap, the decoder asked for a keyframe and dropped every
/// frame until one came, and the late "lost" packet was thrown away when it did
/// arrive. Measured (F24): pli 0 -> 117 with lost 0, ~1.4 keyframes a second on
/// a still screen, and each scroll starting behind a keyframe that held the
/// phone's pacer for 0.5-1.9 s. A packet really missing is still seen as a gap
/// by the next one to arrive, NACKed, and if it does not come back in time the
/// decoder asks for a keyframe as before.
pub(super) fn retransmit_by_media_section(webrtcbin: &gst::Element) {
    let Some(rtpbin) = webrtcbin
        .downcast_ref::<gst::Bin>()
        .and_then(|bin| bin.by_name("rtpbin"))
    else {
        return;
    };
    let webrtcbin = webrtcbin.downgrade();
    rtpbin.connect("new-jitterbuffer", false, move |values| {
        let jitterbuffer = values[1].get::<gst::Element>().ok()?;
        if jitterbuffer.has_property("rtx-next-seqnum", None) {
            jitterbuffer.set_property("rtx-next-seqnum", false);
        }
        let ssrc = values[3].get::<u32>().ok()?;
        let sdp = webrtcbin
            .upgrade()?
            .property::<Option<gst_webrtc::WebRTCSessionDescription>>("remote-description")?
            .sdp()
            .as_text()
            .ok()?;
        let kind = media_kind_of_ssrc(&sdp, ssrc)?;
        if jitterbuffer.has_property("do-retransmission", None) {
            jitterbuffer.set_property("do-retransmission", kind == "video");
        }
        None
    });
}

/// webrtcbin's stats, waiting at most [`STATS_WAIT`].
pub(super) fn webrtcbin_stats(webrtcbin: &gst::Element) -> Option<Vec<gst::Structure>> {
    let (sender, receiver) = mpsc::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let _ = sender.send(reply.ok().flatten().map(|stats| stats.to_owned()));
    });
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    let reply = receiver.recv_timeout(STATS_WAIT).ok().flatten()?;
    Some(
        reply
            .iter()
            .filter_map(|(_, value)| value.get::<gst::Structure>().ok())
            .collect(),
    )
}

/// The local port of the selected ICE pair, from the stats. (Not from the ICE
/// agent: GStreamer 1.28's `GstWebRTCNice` has no `get_selected_pair`, and
/// asking for it aborts the process.)
pub(super) fn selected_local_port(webrtcbin: &gst::Element) -> Option<u16> {
    let stats = webrtcbin_stats(webrtcbin)?;
    let field = |stats: &gst::Structure, name: &str| stats.get::<String>(name).ok();
    let by_id = |id: &str| {
        stats
            .iter()
            .find(|entry| field(entry, "id").as_deref() == Some(id))
    };
    let pair_id = stats
        .iter()
        .find_map(|entry| field(entry, "selected-candidate-pair-id"))?;
    let local = by_id(&field(by_id(&pair_id)?, "local-candidate-id")?)?;
    u16::try_from(local.get::<u32>("port").ok()?).ok()
}

/// Asked of the kernel for the ICE socket's receive buffer. It grants at most
/// twice `net.core.rmem_max` (212992 by default, so 416 KiB) without
/// privileges; asking for more lets a raised sysctl take effect.
const ICE_RECEIVE_BUFFER_BYTES: usize = 4 << 20;

/// Widens the receive buffer of the UDP socket(s) on `port`: libnice leaves
/// its sockets at `rmem_default`, about 100 KB of payload once the kernel's
/// per-packet overhead is counted. Measured on the phone mirror: a 10-15
/// Mbps keyframe burst overflowed it (`Udp RcvbufErrors` rose by 197 while
/// the stream lost 185 packets), and every drop became a PLI and another
/// keyframe. webrtcbin exposes no socket, so this finds it by port among this
/// process's descriptors. Returns the size the kernel reports afterwards.
pub(super) fn widen_receive_buffer(port: u16) -> Option<usize> {
    udp_sockets_on(port)
        .iter()
        .filter_map(|fd| {
            let socket = socket2::SockRef::from(fd);
            socket.set_recv_buffer_size(ICE_RECEIVE_BUFFER_BYTES).ok()?;
            socket.recv_buffer_size().ok()
        })
        .max()
}

/// This process's UDP sockets bound to `port`, duplicated.
pub(super) fn udp_sockets_on(port: u16) -> Vec<std::os::fd::OwnedFd> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        // SAFETY: borrowed only to duplicate it at once; a descriptor closed
        // in between fails the duplication.
        .filter_map(|fd| {
            unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
                .try_clone_to_owned()
                .ok()
        })
        .filter(|fd| {
            let socket = socket2::SockRef::from(fd);
            socket.r#type().ok() == Some(socket2::Type::DGRAM)
                && socket
                    .local_addr()
                    .ok()
                    .and_then(|address| address.as_socket())
                    .is_some_and(|address| address.port() == port)
        })
        .collect()
}

/// The kind (`video`, `audio`) of the m-section that lists `ssrc`.
fn media_kind_of_ssrc(sdp: &str, ssrc: u32) -> Option<&str> {
    let tag = format!("a=ssrc:{ssrc} ");
    let mut kind = None;
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            kind = media.split(' ').next();
        } else if line.starts_with(&tag) {
            return kind;
        }
    }
    None
}

/// How long a stats request may take before the reading is given up.
pub(super) const STATS_WAIT: Duration = Duration::from_secs(2);
