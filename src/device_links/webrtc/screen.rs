//! Screen sharing over the authenticated WebRTC peer.
//!
//! Video is a real WebRTC media track, not data-channel messages: the Android
//! application creates a `VideoTrack` fed by `ScreenCapturerAndroid`, and the
//! `desklink.screen.*` packets are only the control protocol that starts and
//! stops a session. This module owns the desktop half — decoding an inbound
//! track for display, and capturing this desktop for the phone.
//!
//! Both peers carry a `sendrecv` video section from the first offer, so a
//! capture that starts later attaches to a transceiver that already exists.
//! Adding the media section on demand would need renegotiation, and only the
//! deterministic initiator can offer — the responder would have no way to
//! start sharing at all.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use gst::prelude::*;

/// Body value of `role` for "send me the phone's screen".
pub const ROLE_PHONE_SCREEN: &str = "phone-screen";
/// Body value of `role` for "send me the desktop's screen".
pub const ROLE_DESKTOP_SCREEN: &str = "desktop-screen";

/// Codecs for the screen section, in preference order.
///
/// H.264 first because it is what phones encode in **hardware**: nearly every
/// Android device has an H.264 encoder, while hardware VP8 is rare (a recent
/// Snapdragon has none), and libwebrtc's software VP8 drops frames and adds
/// latency at the source. A phone without a hardware H.264 encoder never
/// offers it and stays on VP8. The desktop always sends VP8, which stays
/// negotiated for that direction.
///
/// H.264 is offered only when `h264` says this desktop can decode it
/// ([`h264_decoder_available`]): distributions that ship no H.264 decoder
/// (Fedora without RPM Fusion, Debian without gstreamer1.0-libav) would
/// otherwise agree to a stream they then cannot show.
pub(crate) fn video_caps(h264: bool) -> gst::Caps {
    let vp8 = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "VP8")
        .field("payload", 96i32)
        .field("clock-rate", 90_000i32)
        .field(format!("extmap-{TWCC_OFFER_ID}"), TWCC_URI)
        .build();
    if !h264 {
        return vp8;
    }
    let mut caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("payload", H264_PAYLOAD)
        .field("clock-rate", 90_000i32)
        .field("packetization-mode", "1")
        .field("profile-level-id", "42e01f")
        .field("level-asymmetry-allowed", "1")
        .field(format!("extmap-{TWCC_OFFER_ID}"), TWCC_URI)
        .build();
    caps.merge(vp8);
    caps
}

const H264_PAYLOAD: i32 = 102;

/// Whether any decoder `decodebin` would use accepts `video/x-h264`
/// (avdec_h264, openh264dec, vah264dec, nvh264dec, v4l2slh264dec, ...).
pub(crate) fn h264_decoder_available() -> bool {
    let h264 = gst::Caps::new_empty_simple("video/x-h264");
    gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER | gst::ElementFactoryType::MEDIA_VIDEO,
        gst::Rank::MARGINAL,
    )
    .iter()
    .any(|factory| factory.can_sink_any_caps(&h264))
}

/// Transport-wide congestion control: the sender stamps every packet and the
/// receiver reports arrival times back. Without it libwebrtc has no delay
/// signal, runs loss-only congestion control and holds packets in its pacer —
/// measured at ~100 ms per packet on the phone, with bandwidth to spare.
pub(crate) const TWCC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
/// The id the desktop uses when it offers; when it answers, the offer's id wins.
pub(crate) const TWCC_OFFER_ID: u32 = 3;

/// The TWCC header-extension id of the first video section in `sdp`.
pub(crate) fn twcc_extmap_id(sdp: &str) -> Option<u32> {
    let mut in_video = false;
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            in_video = media.starts_with("video");
            continue;
        }
        if in_video {
            if let Some(rest) = line.strip_prefix("a=extmap:") {
                let mut parts = rest.split_whitespace();
                let id = parts.next()?.split('/').next()?.parse().ok()?;
                if parts.next() == Some(TWCC_URI) {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// What the desktop accepts when it **answers** a phone's offer: hardware
/// H.264 first, VP8 (which the desktop sends) second, nothing else — and TWCC
/// under the offer's id. Without explicit preferences `webrtcbin` echoed every
/// offered codec and dropped every header extension. H.264 only when `h264`
/// (a decoder exists), as in [`video_caps`].
pub(crate) fn answer_video_caps(twcc_id: Option<u32>, h264: bool) -> gst::Caps {
    let codec = |name: &str| {
        let mut builder = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("encoding-name", name)
            .field("clock-rate", 90_000i32);
        if name == "H264" {
            builder = builder.field("packetization-mode", "1");
        }
        if let Some(id) = twcc_id {
            builder = builder.field(format!("extmap-{id}"), TWCC_URI);
        }
        builder.build()
    };
    if !h264 {
        return codec("VP8");
    }
    let mut caps = codec("H264");
    caps.merge(codec("VP8"));
    caps
}

/// Bounds the encoded phone-facing stream while preserving its display aspect
/// ratio. Dimensions are taken from the portal's logical monitor geometry and
/// are untrusted until range-checked.
pub(crate) fn stream_dimensions(
    width: i32,
    height: i32,
    max_dimension: i32,
) -> Result<(i32, i32), String> {
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err("Desktop screen dimensions are invalid".to_string());
    }
    let max_dimension = max_dimension.clamp(320, 1920);
    let scale = (max_dimension as f64 / width.max(height) as f64).min(1.0);
    let scaled_width = ((width as f64 * scale).round() as i32).max(2) & !1;
    let scaled_height = ((height as f64 * scale).round() as i32).max(2) & !1;
    Ok((scaled_width, scaled_height))
}

/// The rendering tail of an inbound screen stream.
///
/// The sink element is held rather than the paintable itself: a
/// `gdk::Paintable` is not `Send`, so it cannot travel from the GStreamer
/// thread that builds this pipeline to the GTK thread that displays it. A
/// `gst::Element` can, and the paintable is read from it on the main thread.
#[derive(Clone)]
pub struct ScreenStream {
    inner: Arc<ScreenStreamInner>,
}

struct ScreenStreamInner {
    pipeline: gst::Pipeline,
    source_pad: gst::Pad,
    elements: Vec<gst::Element>,
    sink: gst::Element,
    stopped: AtomicBool,
    /// Decoded frames that reached the sink.
    shown: Arc<AtomicU64>,
}

impl ScreenStreamInner {
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(decode_sink) = self
            .elements
            .first()
            .and_then(|decode| decode.static_pad("sink"))
        {
            let _ = self.source_pad.unlink(&decode_sink);
        }
        for element in self.elements.iter().rev() {
            let _ = element.set_state(gst::State::Null);
        }
        let _ = self.pipeline.remove_many(self.elements.iter());
        park_pad(&self.pipeline, &self.source_pad);
    }
}

/// Gives an inbound video pad a harmless consumer. The phone keeps sending
/// after the user stops viewing, and RTP reaching an unlinked pad is a fatal
/// `not-linked` error for webrtcbin's shared receive path — which also carries
/// every data channel (max-bundle). Stopping remote control used to kill the
/// whole connection until the 15 s liveness check rebuilt it.
fn park_pad(pipeline: &gst::Pipeline, pad: &gst::Pad) {
    if pad.is_linked() {
        return;
    }
    let Ok(sink) = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
    else {
        return;
    };
    if pipeline.add(&sink).is_err() {
        return;
    }
    let linked = sink
        .static_pad("sink")
        .is_some_and(|target| pad.link(&target).is_ok());
    if !linked || sink.sync_state_with_parent().is_err() {
        let _ = sink.set_state(gst::State::Null);
        let _ = pipeline.remove(&sink);
    }
}

/// Inbound video pads parked by a stopped view, by device, so the next view of
/// the same stream can be shown again. webrtcbin creates the pad once per
/// connection; without this every view after the first stayed blank.
fn parked() -> &'static Mutex<std::collections::HashMap<String, (gst::Pipeline, gst::Pad)>> {
    static PARKED: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, (gst::Pipeline, gst::Pad)>>,
    > = std::sync::OnceLock::new();
    PARKED.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Shows a parked inbound stream again: swaps its fakesink for a fresh decode
/// chain and publishes it. Returns whether there was one.
pub(crate) fn resume_parked(device_id: &str) -> bool {
    let Some((pipeline, pad)) = parked()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(device_id))
    else {
        return false;
    };
    if let Some(peer) = pad.peer() {
        let _ = pad.unlink(&peer);
        if let Some(fake) = peer.parent_element() {
            let _ = fake.set_state(gst::State::Null);
            let _ = pipeline.remove(&fake);
        }
    }
    match attach_incoming_video(&pipeline, &pad) {
        Ok(stream) => {
            publish_stream(device_id, stream);
            true
        }
        Err(error) => {
            eprintln!("[DeskLink] Could not show the phone's screen again: {error}");
            park_pad(&pipeline, &pad);
            false
        }
    }
}

/// The peer is going away: its pads are not worth keeping.
pub(crate) fn forget_parked(device_id: &str) {
    if let Ok(mut map) = parked().lock() {
        map.remove(device_id);
    }
}

impl Drop for ScreenStreamInner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ScreenStream {
    /// The GTK paintable this stream renders into.
    ///
    /// Must be called on the main thread — the returned object belongs to GTK.
    pub fn paintable(&self) -> Option<gtk::gdk::Paintable> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return None;
        }
        let sink = &self.inner.sink;
        sink.has_property("paintable", None)
            .then(|| sink.property::<Option<gtk::gdk::Paintable>>("paintable"))
            .flatten()
    }

    /// Decoded frames delivered to the display so far.
    pub fn frames_shown(&self) -> u64 {
        self.inner.shown.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.inner.stop();
    }
}

impl std::fmt::Debug for ScreenStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScreenStream")
    }
}

/// Streams currently being received, keyed by device.
///
/// Deliberately not part of `DeviceView`: that is serialised each tick for
/// change detection and cannot hold a GStreamer object.
///
/// A process-wide registry rather than a threaded-through handle because the
/// producer is a GStreamer callback several layers below the daemon and the
/// consumer is the GTK main loop, with no shared owner between them — and
/// there is exactly one daemon per process.
pub fn streams() -> &'static Mutex<std::collections::HashMap<String, ScreenStream>> {
    static STREAMS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, ScreenStream>>> =
        std::sync::OnceLock::new();
    STREAMS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Records an inbound stream for a device, replacing any previous one.
pub(crate) fn publish_stream(device_id: &str, stream: ScreenStream) {
    let previous = streams()
        .lock()
        .ok()
        .and_then(|mut streams| streams.insert(device_id.to_string(), stream));
    if let Some(previous) = previous {
        previous.stop();
    }
}

/// Drops a device's stream when its session or screen share ends.
pub(crate) fn clear_stream(device_id: &str) {
    let removed = streams()
        .lock()
        .ok()
        .and_then(|mut streams| streams.remove(device_id));
    if let Some(removed) = removed {
        removed.stop();
        if let Ok(mut map) = parked().lock() {
            map.insert(
                device_id.to_string(),
                (
                    removed.inner.pipeline.clone(),
                    removed.inner.source_pad.clone(),
                ),
            );
        }
    }
}

/// The stream currently available for a device, if any.
pub fn stream_for(device_id: &str) -> Option<ScreenStream> {
    streams().lock().ok()?.get(device_id).cloned()
}

/// Builds the decode-and-render chain for one inbound video pad.
///
/// `webrtcbin` exposes received media as a new source pad carrying RTP.
/// `decodebin` handles depayloading and decoding, then the frames are converted
/// to whatever the sink accepts. Linking happens in `decodebin`'s own
/// `pad-added`, because it cannot know the stream's format until it has parsed
/// some of it.
pub(crate) fn attach_incoming_video(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
) -> Result<ScreenStream, String> {
    let decode = gst::ElementFactory::make("decodebin")
        .build()
        .map_err(|error| format!("DeskLink screen decoding is unavailable: {error}"))?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|error| format!("DeskLink screen conversion is unavailable: {error}"))?;
    // A live mirror shows each frame the moment it is decoded. With clock
    // sync every frame waited for its scheduled time, and as the phone's
    // timestamps drifted against this clock the wait grew: measured at 3–5 s
    // behind the phone, taps landing long before the picture showed them.
    // Pacing smoothness is the jitter buffer's job, not the sink's.
    // Tests have no display for GTK's sink; everything before it is real.
    let sink_factory = if cfg!(test) {
        "fakesink"
    } else {
        "gtk4paintablesink"
    };
    let sink = gst::ElementFactory::make(sink_factory)
        .property("sync", false)
        .build()
        .map_err(|error| {
            format!("DeskLink screen display needs the GStreamer gtk4paintablesink plugin: {error}")
        })?;

    pipeline
        .add_many([&decode, &convert, &sink])
        .map_err(|error| format!("Could not build the DeskLink screen pipeline: {error}"))?;

    // Everything after `add_many` is fallible, and the elements now belong to
    // the pipeline. Returning straight out of any of those steps orphaned all
    // three inside it — and this runs once per `pad-added`, so a repeatedly
    // failing stream leaked a whole decode chain each time. The work is done in
    // a closure so there is exactly one cleanup path.
    let build = || -> Result<(), String> {
        convert
            .link(&sink)
            .map_err(|error| format!("Could not link the DeskLink screen sink: {error}"))?;

        for element in [&convert, &sink] {
            element
                .sync_state_with_parent()
                .map_err(|error| format!("Could not start the DeskLink screen sink: {error}"))?;
        }

        // Loss recovery, by layer. Ordinary Wi-Fi loss is repaired by NACK
        // retransmission (RTCP feedback within ~20 ms). Only when the decoder
        // really cannot decode does it ask for a keyframe, at most twice a
        // second, and it holds the last good picture meanwhile rather than
        // showing grey smear.
        //
        // The depayloader must not ask itself: it requests a keyframe on
        // *every* lost packet, and on Wi-Fi that was ~10 requests a second —
        // each answered with an 84 KB hardware keyframe that backed up the
        // phone's pacer (170–280 ms per packet) and caused more loss.
        let decode_bin = decode
            .downcast_ref::<gst::Bin>()
            .ok_or_else(|| "DeskLink screen decoder is not a bin".to_string())?;
        decode_bin.connect_deep_element_added(|_, _, element| {
            // Only video decoders have this property.
            if element.has_property("automatic-request-sync-points", None) {
                set_if_present(element, "automatic-request-sync-points", "true");
                set_if_present(element, "discard-corrupted-frames", "true");
                set_if_present(element, "min-force-key-unit-interval", "500000000");
                // libav: never show a corrupt frame, and decode with slice
                // threads (frame threads buffer several frames of latency).
                set_if_present(element, "output-corrupt", "false");
                set_if_present(element, "thread-type", "slice");
            }
        });

        let convert_for_decode = convert.clone();
        decode.connect_pad_added(move |_, decoded| {
            let Some(target) = convert_for_decode.static_pad("sink") else {
                return;
            };
            if target.is_linked() {
                return;
            }
            if let Err(error) = decoded.link(&target) {
                eprintln!("[DeskLink] Could not link a decoded screen stream: {error}");
            }
        });

        decode
            .sync_state_with_parent()
            .map_err(|error| format!("Could not start DeskLink screen decoding: {error}"))?;

        let target = decode
            .static_pad("sink")
            .ok_or_else(|| "DeskLink screen decoder has no input".to_string())?;
        pad.link(&target)
            .map_err(|error| format!("Could not attach the DeskLink screen stream: {error}"))?;
        Ok(())
    };

    if let Err(error) = build() {
        for element in [&decode, &convert, &sink] {
            let _ = element.set_state(gst::State::Null);
        }
        let _ = pipeline.remove_many([&decode, &convert, &sink]);
        return Err(error);
    }

    let shown = Arc::new(AtomicU64::new(0));
    if let Some(sink_pad) = sink.static_pad("sink") {
        let counter = Arc::clone(&shown);
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let inner = Arc::new(ScreenStreamInner {
        pipeline: pipeline.clone(),
        source_pad: pad.clone(),
        elements: vec![decode, convert, sink.clone()],
        sink,
        stopped: AtomicBool::new(false),
        shown,
    });
    let weak_inner = Arc::downgrade(&inner);
    pad.connect_unlinked(move |_, _| {
        if let Some(inner) = weak_inner.upgrade() {
            inner.stop();
        }
    });

    Ok(ScreenStream { inner })
}

/// The lifetime-bound source-to-WebRTC chain for a desktop share.
/// Retaining the portal FD and request sink pad is part of the capture: the
/// PipeWire FD must stay open until the source is set to NULL, and the pad is
/// released only after the encoder chain has stopped.
pub(crate) struct DesktopScreenCapture {
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    elements: Vec<gst::Element>,
    sink_pad: gst::Pad,
    /// RTP buffers the payloader has emitted. A capture that is "running" but
    /// still at zero after a few seconds is a black picture on the phone that
    /// nothing else would report.
    frames: Arc<AtomicU64>,
    payload_type: u32,
    _pipewire_fd: Option<OwnedFd>,
    /// Stops the adaptive bitrate controller; see [`spawn_bitrate_controller`].
    rate_control_stop: Arc<AtomicBool>,
}

/// Where a desktop share's frames come from.
pub(crate) enum DesktopSource {
    /// A portal-approved PipeWire stream.
    PipeWire {
        fd: OwnedFd,
        node_id: u32,
        serial: Option<u64>,
    },
    /// The X11 root window, for desktops without a portal (`x11_desktop`).
    X11,
}

impl DesktopScreenCapture {
    pub(crate) fn attach(
        pipeline: &gst::Pipeline,
        webrtcbin: &gst::Element,
        source: DesktopSource,
        width: i32,
        height: i32,
        fps: i32,
        continuity: Option<RtpContinuity>,
    ) -> Result<Self, String> {
        let (source, pipewire_fd) = match source {
            DesktopSource::PipeWire { node_id: 0, .. } => {
                return Err("GNOME portal returned invalid screen stream metadata".to_string());
            }
            DesktopSource::PipeWire {
                fd,
                node_id,
                serial,
            } => (pipewire_source(&fd, node_id, serial)?, Some(fd)),
            DesktopSource::X11 => (x11_source()?, None),
        };
        Self::attach_source(
            pipeline,
            webrtcbin,
            source,
            pipewire_fd,
            width,
            height,
            fps,
            continuity,
        )
    }

    /// Everything after the frame source: scale, encode, packetise, and bind to
    /// the **negotiated** video section. Separate from [`Self::attach`] so the
    /// whole path can be exercised with `videotestsrc` and no display.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_source(
        pipeline: &gst::Pipeline,
        webrtcbin: &gst::Element,
        source: gst::Element,
        pipewire_fd: Option<OwnedFd>,
        width: i32,
        height: i32,
        fps: i32,
        continuity: Option<RtpContinuity>,
    ) -> Result<Self, String> {
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return Err("GNOME portal returned invalid screen stream metadata".to_string());
        }
        let (scale, convert, rate, filter, queue, encoder, payloader) =
            encoder_chain(width, height, fps)?;
        let caps_filter = filter.clone();

        // Scale before converting: colour conversion then runs on the small
        // frame, not the full-resolution one.
        let elements = vec![
            source,
            scale,
            convert,
            rate,
            filter,
            queue,
            encoder.clone(),
            payloader.clone(),
        ];
        pipeline
            .add_many(elements.iter())
            .map_err(|error| format!("Could not add DeskLink screen encoder chain: {error}"))?;
        let frames = Arc::new(AtomicU64::new(0));
        let build = || -> Result<(gst::Pad, u32), String> {
            // Bind to the pad of the **negotiated** video section. When the
            // phone offered, `webrtcbin` has already created that sink pad, and
            // asking for another gets nothing. Otherwise (a previous share
            // released it) the pad is requested *by name for that m-line*: an
            // unnamed request takes the next serial, which creates a fresh,
            // never-negotiated transceiver — media that goes nowhere, with no
            // error to say so.
            let sink_pad = match negotiated_video_sink_pad(webrtcbin) {
                Some(pad) => pad,
                None => {
                    let mline = negotiated_video_mline(webrtcbin).ok_or_else(|| {
                        "The paired device's offer has no video section to send the screen on"
                            .to_string()
                    })?;
                    webrtcbin
                        .request_pad_simple(&format!("sink_{mline}"))
                        .ok_or_else(|| {
                            "Could not attach video to the negotiated WebRTC screen track"
                                .to_string()
                        })?
                }
            };
            let fail = |message: String| {
                webrtcbin.release_request_pad(&sink_pad);
                Err(message)
            };
            // The payload type is whatever was negotiated for VP8, not a number
            // we assume libwebrtc will hand out.
            let negotiated = negotiated_vp8_payload(webrtcbin);
            let payload_type = negotiated.unwrap_or(DEFAULT_VP8_PAYLOAD);
            payloader.set_property("pt", payload_type);
            if let Some(continuity) = continuity {
                payloader.set_property("ssrc", continuity.ssrc);
                if let Some(next) = continuity.next_seqnum {
                    payloader.set_property("seqnum-offset", i32::from(next));
                }
            }
            debug_log!(
                "[DeskLink] Screen chain bound to {} as VP8 pt {payload_type}{}",
                sink_pad.name(),
                if negotiated.is_some() {
                    ""
                } else {
                    " (fallback: VP8 not found in the negotiation)"
                }
            );
            if let Err(error) = gst::Element::link_many(elements.iter()) {
                return fail(format!(
                    "Could not link DeskLink screen encoder chain: {error}"
                ));
            }
            let Some(src_pad) = elements
                .last()
                .and_then(|element| element.static_pad("src"))
            else {
                return fail("DeskLink VP8 payloader has no output pad".to_string());
            };
            // RTP payloaders push `GstBufferList`s, and a plain BUFFER probe never
            // fires for those — it counted zero packets from a perfectly
            // healthy chain. Count both, by packet.
            let counter = Arc::clone(&frames);
            src_pad.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let packets = match &info.data {
                        Some(gst::PadProbeData::Buffer(_)) => 1,
                        Some(gst::PadProbeData::BufferList(list)) => list.len() as u64,
                        _ => 0,
                    };
                    counter.fetch_add(packets, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                },
            );
            // A chain that dies (its PipeWire node gone, a replaced share)
            // sends EOS. Into webrtcbin's shared, max-bundled sink that ends
            // the screen track for good, and with the reused `screen_ssrc` the
            // phone's picture froze. Shares end by being unlinked, never by EOS.
            src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
                match &info.data {
                    Some(gst::PadProbeData::Event(event))
                        if event.type_() == gst::EventType::Eos =>
                    {
                        gst::PadProbeReturn::Drop
                    }
                    _ => gst::PadProbeReturn::Ok,
                }
            });
            if let Err(error) = src_pad.link(&sink_pad) {
                return fail(format!(
                    "Could not attach the DeskLink screen RTP track: {error}"
                ));
            }
            // Downstream first: an upstream element going PLAYING into a
            // downstream one still flushing stalled the chain (FLUSHING).
            for element in elements.iter().rev() {
                if let Err(error) = element.sync_state_with_parent() {
                    return fail(format!("Could not start DeskLink screen capture: {error}"));
                }
            }
            Ok((sink_pad, payload_type))
        };
        match build() {
            Ok((sink_pad, payload_type)) => Ok(Self {
                rate_control_stop: spawn_bitrate_controller(webrtcbin, &encoder, &caps_filter),
                pipeline: pipeline.clone(),
                webrtcbin: webrtcbin.clone(),
                elements,
                sink_pad,
                frames,
                payload_type,
                _pipewire_fd: pipewire_fd,
            }),
            Err(error) => {
                for element in &elements {
                    let _ = element.set_state(gst::State::Null);
                }
                let _ = pipeline.remove_many(elements.iter());
                Err(error)
            }
        }
    }

    /// RTP buffers produced so far.
    /// The RTP sequence number the next share must start at, once this one
    /// has sent anything.
    pub(crate) fn next_seqnum(&self) -> Option<u16> {
        if self.frames_sent() == 0 {
            return None;
        }
        self.elements
            .last()
            .filter(|payloader| payloader.has_property("seqnum", None))
            .map(|payloader| (payloader.property::<u32>("seqnum") as u16).wrapping_add(1))
    }

    /// The screen's real pixel size, as the capture negotiated it (a scaled
    /// laptop panel: 2560x1600 behind a 1920x1200 logical screen).
    pub(crate) fn source_size(&self) -> Option<(i32, i32)> {
        let caps = self.elements.first()?.static_pad("src")?.current_caps()?;
        let structure = caps.structure(0)?;
        Some((
            structure.get::<i32>("width").ok()?,
            structure.get::<i32>("height").ok()?,
        ))
    }

    pub(crate) fn frames_sent(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// The VP8 payload type this capture packetises with.
    pub(crate) fn payload_type(&self) -> u32 {
        self.payload_type
    }

    /// Stops the capture while its peer stays alive.
    ///
    /// Returns `Err(self)` when the chain is **parked** and could not be wound
    /// down. While the connection is not ready `webrtcbin` holds the source's
    /// streaming thread inside its own send path, and setting the source to NULL
    /// waits for that thread to exit — forever, because only tearing `webrtcbin`
    /// down (or the transport coming up) frees it. Waiting would wedge whichever
    /// thread ended the share, and with it the whole peer. So this proceeds only
    /// once the chain's output pad is idle; otherwise the caller keeps the
    /// capture (see the peer's `retired_captures`) and the peer's `close`, which
    /// sets the whole pipeline to NULL with `webrtcbin` first, releases it.
    pub(crate) fn stop(self) -> Result<(), Self> {
        self.rate_control_stop.store(true, Ordering::Relaxed);
        // Unlink from `webrtcbin` first so nothing new can enter it. The pad is
        // deliberately *kept*, not released: `webrtcbin` builds the sender chain
        // (including its FEC/RED encoders) when the pad is requested, and asking
        // for the same section's pad again a second time logs a CRITICAL and
        // leaves that chain half-built. The next share finds this pad again.
        if let Some(payloader_src) = self
            .elements
            .last()
            .and_then(|element| element.static_pad("src"))
        {
            let _ = payloader_src.unlink(&self.sink_pad);
        }
        if !self.wait_until_idle(IDLE_TIMEOUT) {
            return Err(self);
        }
        for element in &self.elements {
            let _ = element.set_state(gst::State::Null);
        }
        let _ = self.pipeline.remove_many(self.elements.iter());
        Ok(())
    }

    /// True once no buffer is in flight through the payloader's output pad.
    fn wait_until_idle(&self, timeout: std::time::Duration) -> bool {
        let Some(pad) = self
            .elements
            .last()
            .and_then(|element| element.static_pad("src"))
        else {
            return true;
        };
        let (idle, waiting) = std::sync::mpsc::channel();
        // An IDLE probe runs when the pad has no data passing through it: at
        // once for a quiet chain, or as soon as a parked thread gets back out.
        let Some(probe) = pad.add_probe(gst::PadProbeType::IDLE, move |_, _| {
            let _ = idle.send(());
            gst::PadProbeReturn::Remove
        }) else {
            return true;
        };
        if waiting.recv_timeout(timeout).is_ok() {
            true
        } else {
            pad.remove_probe(probe);
            false
        }
    }

    /// Drops the capture without touching its elements, for use once the whole
    /// pipeline has already been set to NULL by the peer's `close`.
    pub(crate) fn abandon(self) {
        drop(self);
    }
}

/// How a new share continues the RTP stream of the previous one on the same
/// connection: same SSRC, sequence numbers carrying on. The phone binds its
/// picture to the first SSRC it sees, and SRTP refuses to encrypt a sequence
/// number it has already used for that SSRC ("general supporting library
/// error" in srtpenc) — so both are needed, or a reopened Remote input waits
/// for its first picture forever.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RtpContinuity {
    pub ssrc: u32,
    pub next_seqnum: Option<u16>,
}

/// Longest [`DesktopScreenCapture::stop`] waits for the chain to go idle.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// VP8's payload type when the peer's offer does not make one visible.
const DEFAULT_VP8_PAYLOAD: u32 = 96;

/// Applies a property given as a string, only when the element has it. The
/// string form avoids guessing an enum's or flags' Rust type; the guard is
/// because `set_property` panics on an absent name and GStreamer plugins differ
/// between versions.
/// Encoder bitrate bounds and start point for the desktop screen.
const START_BITRATE: u32 = 2_500_000;
const MIN_BITRATE: u32 = 400_000;
const MAX_BITRATE: u32 = 8_000_000;

/// AIMD on the loss the phone reports in its RTCP receiver reports: back off
/// hard under loss, probe up gently when clean. The encoder used to sit at a
/// fixed 2 Mbps, so a weak link built latency instead of dropping quality.
pub(crate) fn next_bitrate(current: u32, fraction_lost: f64) -> u32 {
    let next = if fraction_lost > 0.10 {
        current / 10 * 7
    } else if fraction_lost < 0.02 {
        current / 10 * 11
    } else {
        current
    };
    next.clamp(MIN_BITRATE, MAX_BITRATE)
}

/// Frame rate under sustained loss at the bitrate floor, and normally.
const REDUCED_FPS: i32 = 15;
const FULL_FPS: i32 = 30;
/// Seconds of a condition before the frame rate changes, so it never flaps.
const FPS_HOLD_SECONDS: u32 = 5;

/// The second rung after bitrate: when even the lowest bitrate still loses
/// packets, fewer frames give each one more bits (text stays sharp, motion
/// gets choppier); when the link is clean again with room to spare, the full
/// rate returns. Counts consecutive seconds of each condition in `streak`.
pub(crate) fn next_fps(current: i32, bitrate: u32, fraction_lost: f64, streak: &mut u32) -> i32 {
    let wants_lower = current == FULL_FPS && bitrate == MIN_BITRATE && fraction_lost > 0.02;
    let wants_higher =
        current == REDUCED_FPS && bitrate >= START_BITRATE / 2 && fraction_lost < 0.02;
    if wants_lower || wants_higher {
        *streak += 1;
    } else {
        *streak = 0;
    }
    if *streak < FPS_HOLD_SECONDS {
        return current;
    }
    *streak = 0;
    if wants_lower {
        REDUCED_FPS
    } else {
        FULL_FPS
    }
}

/// Once a second, reads the peer-reported loss from `webrtcbin` stats and
/// steers the encoder's bitrate, then the frame rate — through the caps
/// filter's `framerate`, which `videorate` follows. (Lowering `videorate`'s
/// `max-rate` under caps that pin 30/1 failed with `not-negotiated`.) Holds
/// only weak references, so it ends with the pipeline; the returned flag ends
/// it with the capture.
fn spawn_bitrate_controller(
    webrtcbin: &gst::Element,
    encoder: &gst::Element,
    filter: &gst::Element,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let (webrtcbin, encoder, filter, flag) = (
        webrtcbin.downgrade(),
        encoder.downgrade(),
        filter.downgrade(),
        Arc::clone(&stop),
    );
    let spawned = std::thread::Builder::new()
        .name("desklink-screen-rate".to_string())
        .spawn(move || {
            let mut bitrate = START_BITRATE;
            let mut fps = FULL_FPS;
            let mut streak = 0;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let (Some(webrtcbin), Some(encoder)) = (webrtcbin.upgrade(), encoder.upgrade())
                else {
                    break;
                };
                let Some(lost) = peer_reported_loss(&webrtcbin) else {
                    continue;
                };
                let next = next_bitrate(bitrate, lost);
                if next != bitrate {
                    bitrate = next;
                    encoder.set_property("target-bitrate", bitrate as i32);
                }
                let next = next_fps(fps, bitrate, lost, &mut streak);
                if next != fps {
                    fps = next;
                    if let Some(filter) = filter.upgrade() {
                        let caps = filter.property::<gst::Caps>("caps");
                        filter.set_property("caps", with_framerate(&caps, fps));
                    }
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("[DeskLink] Screen bitrate control unavailable: {error}");
    }
    stop
}

/// The worst `fraction-lost` any remote-inbound-rtp stats entry reports.
fn peer_reported_loss(webrtcbin: &gst::Element) -> Option<f64> {
    let promise = gst::Promise::new();
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    if promise.wait() != gst::PromiseResult::Replied {
        return None;
    }
    promise
        .get_reply()?
        .iter()
        .filter_map(|(_, value)| value.get::<gst::Structure>().ok())
        .filter_map(|stats| stats.get::<f64>("fraction-lost").ok())
        .reduce(f64::max)
}

/// `caps` with its frame rate set to `fps`/1, everything else kept.
fn with_framerate(caps: &gst::Caps, fps: i32) -> gst::Caps {
    let mut caps = caps.copy();
    if let Some(structure) = caps.make_mut().structure_mut(0) {
        structure.set("framerate", gst::Fraction::new(fps, 1));
    }
    caps
}

fn set_if_present(element: &gst::Element, name: &str, value: &str) {
    if element.has_property(name, None) {
        element.set_property_from_str(name, value);
    }
}

/// Everything between a frame source and RTP: scale, convert, constant rate,
/// leaky queue, low-latency VP8, payloader — in that order.
#[allow(clippy::type_complexity)]
fn encoder_chain(
    width: i32,
    height: i32,
    fps: i32,
) -> Result<
    (
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
        gst::Element,
    ),
    String,
> {
    // Keep desktop encoding bounded on high-resolution displays. The
    // `width`/`height` arguments remain the compositor's logical geometry
    // for pointer mapping; only the encoded video is scaled, preserving
    // its aspect ratio.
    let (capture_width, capture_height) = stream_dimensions(width, height, 1280)?;
    let fps = fps.clamp(1, 30);

    let make = |factory: &str, what: &str| {
        gst::ElementFactory::make(factory)
            .build()
            .map_err(|error| format!("DeskLink screen capture needs GStreamer {what}: {error}"))
    };
    let convert = make("videoconvert", "videoconvert")?;
    let scale = make("videoscale", "videoscale")?;
    // Constant frame rate downstream of a source that only emits on change.
    let rate = make("videorate", "videorate")?;
    let filter = make("capsfilter", "capsfilter")?;
    // One frame, leaky: if the encoder falls behind, the *newest* frame
    // wins instead of a queue of stale ones adding latency.
    let queue = make("queue", "queue")?;
    queue.set_property("max-size-buffers", 1u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");
    let encoder = make("vp8enc", "vp8enc")?;
    let payloader = make("rtpvp8pay", "rtpvp8pay")?;

    filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", capture_width)
            .field("height", capture_height)
            .field("framerate", gst::Fraction::new(fps, 1))
            .build(),
    );
    set_if_present(&rate, "skip-to-first", "true");
    encoder.set_property("deadline", 1i64);
    encoder.set_property("cpu-used", 8i32);
    encoder.set_property("target-bitrate", START_BITRATE as i32);
    // Keyframes come on the decoder's request (throttled); this is only the
    // safety net for a request that never arrives. Every 2 s was a burst of
    // screen-sized keyframes on a link that was already struggling.
    encoder.set_property("keyframe-max-dist", fps * 10);
    // Low-latency rate control: libvpx defaults buffer 4–6 s of data,
    // which lets bursts queue instead of shrinking quality.
    set_if_present(&encoder, "threads", "4");
    set_if_present(&encoder, "buffer-size", "150");
    set_if_present(&encoder, "buffer-initial-size", "100");
    set_if_present(&encoder, "buffer-optimal-size", "120");
    // Real-time rate control, and the tuning libvpx recommends for screen
    // content: skip blocks that did not change.
    set_if_present(&encoder, "end-usage", "cbr");
    set_if_present(&encoder, "error-resilient", "default");
    set_if_present(&encoder, "static-threshold", "100");
    // libwebrtc's VP8 depacketiser recovers from loss with 15-bit picture
    // ids; without them a lost packet costs a visible stall.
    set_if_present(&payloader, "picture-id-mode", "15-bit");
    Ok((scale, convert, rate, filter, queue, encoder, payloader))
}

/// Debug builds: the real desktop-share path without a phone — GNOME portal
/// (resuming `device_id`'s remembered grant, or asking at the desktop),
/// PipeWire capture of this screen, the exact encoder chain the phone gets,
/// and one pointer nudge through the portal. Blocks for a few seconds.
#[cfg(debug_assertions)]
pub(crate) fn desktop_share_selftest(device_id: &str) -> Result<String, String> {
    use super::remote_desktop_portal::{DesktopSession, RemoteDesktopPortal};
    gst::init().map_err(|error| error.to_string())?;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let stored = crate::device_links::config::remote_desktop_token(device_id);
    let resumed = stored.is_some();
    let started = std::time::Instant::now();
    let (portal, source) = RemoteDesktopPortal::start("", &cancel, stored.as_deref())?;
    let consent = started.elapsed();
    crate::device_links::config::set_remote_desktop_token(device_id, source.token_to_remember());
    let remembered = source.restore_token.is_some();

    let pipeline = gst::Pipeline::new();
    let capture = pipewire_source(&source.fd, source.node_id, source.pipewire_serial)?;
    let (scale, convert, rate, filter, queue, encoder, payloader) =
        encoder_chain(source.width, source.height, 30)?;
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .map_err(|error| error.to_string())?;
    let chain = [
        capture,
        scale,
        convert,
        rate,
        filter,
        queue,
        encoder,
        payloader.clone(),
        sink,
    ];
    pipeline
        .add_many(chain.iter())
        .map_err(|error| error.to_string())?;
    gst::Element::link_many(chain.iter()).map_err(|error| error.to_string())?;
    let packets = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&packets);
    payloader
        .static_pad("src")
        .ok_or("payloader has no src pad")?
        .add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            },
        );
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("capture pipeline would not start: {error}"))?;
    let mut first_packet = None;
    let mut bus_error = None;
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    while started.elapsed() < consent + std::time::Duration::from_secs(4) {
        if first_packet.is_none() && packets.load(Ordering::Relaxed) > 0 {
            first_packet = Some(started.elapsed() - consent);
        }
        if let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            if let gst::MessageView::Error(error) = message.view() {
                bus_error = Some(format!("{} ({:?})", error.error(), error.debug()));
                break;
            }
        }
    }
    let total = packets.load(Ordering::Relaxed);
    let _ = pipeline.set_state(gst::State::Null);
    let input = portal
        .pointer_motion(1.0, 0.0)
        .and_then(|_| portal.pointer_motion(-1.0, 0.0));
    drop(portal);
    if let Some(error) = bus_error {
        return Err(format!("capture failed after {total} packets: {error}"));
    }
    Ok(format!(
        "screen {}x{}, devices granted={} (3 = keyboard+pointer), grant {} in {:.1}s, \
         remembered={remembered}, first RTP after {:?}, {total} RTP packet groups in 4s, input={}",
        source.width,
        source.height,
        source.granted_device_types,
        if resumed { "resumed" } else { "asked" },
        consent.as_secs_f32(),
        first_packet,
        input
            .map(|_| "ok".to_string())
            .unwrap_or_else(|error| error),
    ))
}

/// The PipeWire frame source for a portal-approved stream.
fn pipewire_source(
    pipewire_fd: &OwnedFd,
    node_id: u32,
    pipewire_serial: Option<u64>,
) -> Result<gst::Element, String> {
    // Unnamed: a fixed name collided with the previous share's source while
    // it was still being removed, and every reopen failed to add the chain.
    let source = gst::ElementFactory::make("pipewiresrc")
        .build()
        .map_err(|error| format!("DeskLink screen capture needs GStreamer pipewiresrc: {error}"))?;
    if !source.has_property("fd", None) {
        return Err("GStreamer pipewiresrc lacks portal FD support".to_string());
    }
    source.set_property("client-name", "DeskLink");
    source.set_property("fd", pipewire_fd.as_raw_fd());
    // Portal v6 stream serials are never reused; older portals only return the
    // node id, which we pass through pipewiresrc's deprecated path.
    if let Some(serial) = pipewire_serial.filter(|_| source.has_property("target-object", None)) {
        source.set_property("target-object", serial.to_string());
    } else if source.has_property("path", None) {
        source.set_property("path", node_id.to_string());
    } else {
        return Err("GStreamer pipewiresrc cannot target a portal screen stream".to_string());
    }
    source.set_property("do-timestamp", true);
    // A compositor sends a frame only when something changes, so a static
    // desktop would produce no frames at all: nothing to encode, no keyframe to
    // answer a viewer's request with, a black picture. Re-send the last frame.
    set_if_present(&source, "keepalive-time", "500");
    Ok(source)
}

/// The X11 root window as a frame source. Damage tracking is off: it costs
/// a round trip per damaged rectangle and the encoder wants whole frames.
fn x11_source() -> Result<gst::Element, String> {
    gst::ElementFactory::make("ximagesrc")
        .property("use-damage", false)
        .property("show-pointer", true)
        .build()
        .map_err(|error| {
            format!("DeskLink screen capture on X11 needs GStreamer ximagesrc (gst-plugins-good): {error}")
        })
}

/// The unlinked sink pad `webrtcbin` already holds for the negotiated video
/// section, if it has one.
fn negotiated_video_sink_pad(webrtcbin: &gst::Element) -> Option<gst::Pad> {
    webrtcbin.sink_pads().into_iter().find(|pad| {
        !pad.is_linked()
            && pad.find_property("transceiver").is_some()
            && pad
                .property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver")
                .is_some_and(|transceiver| transceiver.kind() == gst_webrtc::WebRTCKind::Video)
    })
}

/// The m-line index of the video section the remote offer negotiated.
fn negotiated_video_mline(webrtcbin: &gst::Element) -> Option<u32> {
    for index in 0i32..16 {
        let transceiver = webrtcbin.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
            "get-transceiver",
            &[&index],
        )?;
        if transceiver.kind() == gst_webrtc::WebRTCKind::Video {
            let mline = transceiver.property::<u32>("mlineindex");
            if mline != u32::MAX {
                return Some(mline);
            }
        }
    }
    None
}

/// The VP8 payload type in the negotiated local description's video section.
///
/// Reading it from the SDP is deterministic where the sink pad's caps are not
/// (they only carry a payload *range*). An answerer must reuse the offerer's
/// payload type, and an offerer's own list is what the answerer maps to.
fn negotiated_vp8_payload(webrtcbin: &gst::Element) -> Option<u32> {
    let description =
        webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")?;
    let sdp = description.sdp();
    for media in sdp.medias() {
        if media.media() != Some("video") {
            continue;
        }
        for attribute in media.attributes() {
            if attribute.key() != "rtpmap" {
                continue;
            }
            // `<pt> <codec>/<clock rate>[/<channels>]`
            let mut parts = attribute.value()?.split_whitespace();
            let (Some(payload), Some(codec)) = (parts.next(), parts.next()) else {
                continue;
            };
            if codec
                .split('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("VP8"))
            {
                return payload
                    .parse::<u32>()
                    .ok()
                    .filter(|pt| (96..=127).contains(pt));
            }
        }
    }
    None
}

/// Parameters a peer asked for when requesting a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRequest {
    pub max_dimension: i32,
    pub fps: i32,
}

impl ScreenRequest {
    /// Reads a request body, clamping to what this desktop will actually serve.
    ///
    /// The values arrive from the paired device, so they are bounds-checked
    /// rather than trusted: an unclamped `fps` or dimension would size a real
    /// encoder pipeline from a remote number.
    pub fn from_body(packet: &crate::device_links::packet::NetworkPacket) -> Self {
        let max_dimension = packet
            .get_i64("maxDimension")
            .unwrap_or(1280)
            .clamp(320, 1920) as i32;
        let fps = packet.get_i64("fps").unwrap_or(15).clamp(1, 30) as i32;
        Self { max_dimension, fps }
    }
}

impl Default for ScreenRequest {
    fn default() -> Self {
        Self {
            max_dimension: 1280,
            fps: 15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_rate_changes_in_the_caps_and_nothing_else_does() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", 1280)
            .field("height", 800)
            .field("framerate", gst::Fraction::new(30, 1))
            .build();
        let slower = with_framerate(&caps, 15);
        let structure = slower.structure(0).unwrap();
        assert_eq!(
            structure.get::<gst::Fraction>("framerate").unwrap(),
            gst::Fraction::new(15, 1)
        );
        assert_eq!(structure.get::<i32>("width").unwrap(), 1280);
        assert_eq!(structure.get::<&str>("format").unwrap(), "I420");
        assert_eq!(
            caps.structure(0)
                .unwrap()
                .get::<gst::Fraction>("framerate")
                .unwrap(),
            gst::Fraction::new(30, 1),
            "the original is untouched"
        );
    }

    #[test]
    fn bitrate_backs_off_under_loss_probes_up_when_clean_and_stays_bounded() {
        assert!(next_bitrate(START_BITRATE, 0.20) < START_BITRATE);
        assert!(next_bitrate(START_BITRATE, 0.0) > START_BITRATE);
        assert_eq!(next_bitrate(START_BITRATE, 0.05), START_BITRATE);
        assert_eq!(next_bitrate(MIN_BITRATE, 0.5), MIN_BITRATE);
        assert_eq!(next_bitrate(MAX_BITRATE, 0.0), MAX_BITRATE);
        // Sustained heavy loss reaches the floor in a handful of seconds.
        let floor = (0..10).fold(START_BITRATE, |rate, _| next_bitrate(rate, 0.5));
        assert_eq!(floor, MIN_BITRATE);
    }

    #[test]
    fn frame_rate_drops_only_after_sustained_loss_at_the_floor_and_recovers() {
        let mut streak = 0;
        // Loss above the floor is the bitrate's job, not the frame rate's.
        for _ in 0..10 {
            assert_eq!(
                next_fps(FULL_FPS, START_BITRATE, 0.3, &mut streak),
                FULL_FPS
            );
        }
        let mut fps = FULL_FPS;
        for second in 1..=FPS_HOLD_SECONDS {
            fps = next_fps(fps, MIN_BITRATE, 0.3, &mut streak);
            let expected = if second < FPS_HOLD_SECONDS {
                FULL_FPS
            } else {
                REDUCED_FPS
            };
            assert_eq!(fps, expected);
        }
        // A clean second in between resets the count: no flapping.
        assert_eq!(next_fps(fps, START_BITRATE, 0.0, &mut streak), REDUCED_FPS);
        assert_eq!(next_fps(fps, START_BITRATE, 0.3, &mut streak), REDUCED_FPS);
        for _ in 0..FPS_HOLD_SECONDS {
            fps = next_fps(fps, START_BITRATE, 0.0, &mut streak);
        }
        assert_eq!(fps, FULL_FPS);
    }
    use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SCREEN_REQUEST};

    #[test]
    fn screen_request_clamps_values_supplied_by_the_peer() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
        packet.set("maxDimension", 99_999);
        packet.set("fps", 240);

        let request = ScreenRequest::from_body(&packet);

        assert_eq!(request.max_dimension, 1920);
        assert_eq!(request.fps, 30);
    }

    #[test]
    fn screen_request_falls_back_to_defaults() {
        let packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
        assert_eq!(ScreenRequest::from_body(&packet), ScreenRequest::default());
    }

    #[test]
    fn the_twcc_id_is_read_from_the_video_section_only() {
        let sdp = "v=0\r\nm=audio 9 X 111\r\na=extmap:9 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
                   m=video 9 X 96\r\na=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
                   a=extmap:4/sendrecv http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
        assert_eq!(twcc_extmap_id(sdp), Some(4));
        assert_eq!(twcc_extmap_id("v=0\r\nm=video 9 X 96\r\n"), None);
    }

    #[test]
    fn the_answer_accepts_h264_then_vp8_with_the_offers_twcc_id() {
        gst::init().unwrap();
        let caps = answer_video_caps(Some(4), true);
        let names: Vec<_> = caps
            .iter()
            .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
            .collect();
        assert_eq!(names, ["H264", "VP8"]);
        assert!(caps
            .iter()
            .all(|s| s.get::<&str>("extmap-4").unwrap() == TWCC_URI));
        // No payload numbers: the offer's must win in the intersection.
        assert!(caps.iter().all(|s| !s.has_field("payload")));
        assert!(answer_video_caps(None, true)
            .iter()
            .all(|s| !s.has_field("extmap-4")));
    }

    #[test]
    fn without_an_h264_decoder_only_vp8_is_offered_or_answered() {
        gst::init().unwrap();
        let names = |caps: gst::Caps| -> Vec<String> {
            caps.iter()
                .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
                .collect()
        };
        assert_eq!(names(video_caps(false)), ["VP8"]);
        assert_eq!(names(answer_video_caps(Some(4), false)), ["VP8"]);
        // The registry probe itself must not panic, whatever is installed.
        let _ = h264_decoder_available();
    }

    #[test]
    fn video_caps_prefer_hardware_h264_and_keep_vp8_for_sending() {
        // Building caps touches the GStreamer type system, so this test needs
        // the library initialised even though it starts no pipeline.
        gst::init().unwrap();

        let caps = video_caps(true);
        let names: Vec<_> = caps
            .iter()
            .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
            .collect();
        assert_eq!(names, ["H264", "VP8"]);
        let h264 = caps.structure(0).unwrap();
        assert_eq!(h264.get::<&str>("packetization-mode").unwrap(), "1");
        assert!(caps
            .iter()
            .all(|s| s.get::<&str>("media").unwrap() == "video"));
    }

    #[test]
    fn desktop_capture_dimensions_preserve_aspect_and_bound_encoding_cost() {
        assert_eq!(stream_dimensions(2560, 1600, 1280).unwrap(), (1280, 800));
        assert_eq!(stream_dimensions(1080, 2400, 1280).unwrap(), (576, 1280));
        assert!(stream_dimensions(0, 1080, 1280).is_err());
    }

    /// The decode chain goes, and the pad is parked on a fakesink rather than
    /// left unlinked (RTP into an unlinked pad kills webrtcbin's receive path).
    fn is_parked(pad: &gst::Pad) -> bool {
        pad.peer()
            .and_then(|peer| peer.parent_element())
            .and_then(|element| element.factory())
            .is_some_and(|factory| factory.name() == "fakesink")
    }

    #[test]
    fn stopping_an_inbound_stream_removes_its_decode_chain_and_parks_the_pad() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&source).unwrap();
        let source_pad = source.static_pad("src").unwrap();
        let stream = attach_incoming_video(&pipeline, &source_pad).unwrap();
        publish_stream("phone-stop", stream);
        assert_eq!(pipeline.children().len(), 4);

        clear_stream("phone-stop");

        assert_eq!(pipeline.children().len(), 2, "identity + parking fakesink");
        assert!(is_parked(&source_pad));
        forget_parked("phone-stop");
    }

    #[test]
    fn a_stopped_view_is_shown_again_on_the_same_pad() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&source).unwrap();
        let source_pad = source.static_pad("src").unwrap();
        publish_stream(
            "phone-again",
            attach_incoming_video(&pipeline, &source_pad).unwrap(),
        );
        clear_stream("phone-again");

        assert!(resume_parked("phone-again"));
        assert!(stream_for("phone-again").is_some());
        assert_eq!(
            pipeline.children().len(),
            4,
            "decode chain back, fakesink gone"
        );
        assert!(!is_parked(&source_pad));
        assert!(!resume_parked("phone-again"), "nothing parked any more");
        clear_stream("phone-again");
        forget_parked("phone-again");
    }

    #[test]
    fn replacing_an_inbound_stream_stops_the_previous_decode_chain() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let first_source = gst::ElementFactory::make("identity").build().unwrap();
        let second_source = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add_many([&first_source, &second_source]).unwrap();
        let first_pad = first_source.static_pad("src").unwrap();
        let first = attach_incoming_video(&pipeline, &first_pad).unwrap();
        publish_stream("phone-replace", first);
        let second =
            attach_incoming_video(&pipeline, &second_source.static_pad("src").unwrap()).unwrap();
        publish_stream("phone-replace", second);

        // first chain replaced by a parking fakesink; second chain live.
        assert_eq!(pipeline.children().len(), 6);
        assert!(is_parked(&first_pad));
        clear_stream("phone-replace");
        assert_eq!(pipeline.children().len(), 4);
        forget_parked("phone-replace");
    }

    #[test]
    fn unlinked_inbound_track_stops_its_decode_chain() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&source).unwrap();
        let source_pad = source.static_pad("src").unwrap();
        let stream = attach_incoming_video(&pipeline, &source_pad).unwrap();
        let decode_sink = stream.inner.elements[0].static_pad("sink").unwrap();

        source_pad.unlink(&decode_sink).unwrap();

        assert_eq!(pipeline.children().len(), 2, "identity + parking fakesink");
        assert!(is_parked(&source_pad));
    }
}
