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
        .field(
            "profile-level-id",
            super::peer_connection::H264_RECEIVE_LEVEL,
        )
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
    let max_dimension = max_dimension.clamp(MIN_DIMENSION as i32, MAX_DIMENSION as i32);
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

/// Whether `decodebin` chose a VA-API decoder (whose DMABuf output the GTK
/// sink takes without a copy), unless turned off for a regression hunt.
fn zero_copy_wanted(decodebin: &gst::Element) -> bool {
    if cfg!(test) || std::env::var("DESKLINK_SCREEN_COPY").as_deref() == Ok("1") {
        return false;
    }
    let Some(bin) = decodebin.downcast_ref::<gst::Bin>() else {
        return false;
    };
    bin.iterate_recurse().into_iter().flatten().any(|element| {
        element.factory().is_some_and(|factory| {
            factory.name().starts_with("va") && factory.has_type(gst::ElementFactoryType::DECODER)
        })
    })
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

        // Zero-copy when the decoder is VA-API: its frames go to the sink as
        // DMABuf (GTK imports them; no download to system memory and upload
        // again). A software decoder's frames go through `videoconvert`.
        // `DESKLINK_SCREEN_COPY=1` forces the converting path.
        let (convert_for_decode, sink_for_decode) = (convert.clone(), sink.clone());
        decode.connect_pad_added(move |bin, decoded| {
            let direct = zero_copy_wanted(bin);
            let linked = if direct {
                sink_for_decode
                    .static_pad("sink")
                    .ok_or_else(|| "no sink pad".to_string())
                    .and_then(|target| decoded.link(&target).map_err(|error| error.to_string()))
            } else {
                convert_for_decode
                    .link(&sink_for_decode)
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        let target = convert_for_decode
                            .static_pad("sink")
                            .ok_or_else(|| "no convert pad".to_string())?;
                        decoded.link(&target).map_err(|error| error.to_string())
                    })
            };
            match linked {
                Ok(_) => debug_log!(
                    "[DeskLink] Phone screen decoded {}",
                    if direct {
                        "by VA-API straight to the display (DMABuf)"
                    } else {
                        "through videoconvert"
                    }
                ),
                Err(error) => {
                    eprintln!("[DeskLink] Could not link a decoded screen stream: {error}")
                }
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
    /// Scaled by `vapostproc`: if it sends nothing, VA is blamed.
    va_scaled: bool,
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
        request: ScreenRequest,
        continuity: Option<RtpContinuity>,
    ) -> Result<Self, String> {
        let (source, pipewire_fd) = match source {
            DesktopSource::PipeWire { node_id: 0, .. } => {
                return Err(
                    "The desktop portal returned invalid screen stream metadata".to_string()
                );
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
            request,
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
        request: ScreenRequest,
        continuity: Option<RtpContinuity>,
    ) -> Result<Self, String> {
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return Err("The desktop portal returned invalid screen stream metadata".to_string());
        }
        let request = request.served(width, height);
        let (scale, convert, rate, filter, queue, encoder, payloader) =
            encoder_chain(width, height, request)?;
        let va_scaled = scale
            .factory()
            .is_some_and(|factory| factory.name() == "vapostproc");

        // Scale before converting: colour conversion then runs on the small
        // frame, not the full-resolution one.
        let elements = vec![
            source,
            scale,
            convert,
            rate.clone(),
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
            // The payload type is whatever was negotiated for the codec, not a
            // number we assume libwebrtc will hand out.
            let codec = if h264_experiment() { "H264" } else { "VP8" };
            let negotiated = negotiated_payload(webrtcbin, codec);
            if codec == "H264" && negotiated.is_none() {
                return fail("The phone did not negotiate H.264 (experiment)".to_string());
            }
            let payload_type = negotiated.unwrap_or(DEFAULT_VP8_PAYLOAD);
            payloader.set_property("pt", payload_type);
            if let Some(continuity) = continuity {
                payloader.set_property("ssrc", continuity.ssrc);
                if let Some(next) = continuity.next_seqnum {
                    payloader.set_property("seqnum-offset", i32::from(next));
                }
            }
            debug_log!(
                "[DeskLink] Screen chain bound to {} as {codec} pt {payload_type}{}",
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
                rate_control_stop: spawn_bitrate_controller(
                    webrtcbin,
                    &encoder,
                    &rate,
                    request.fps,
                ),
                pipeline: pipeline.clone(),
                webrtcbin: webrtcbin.clone(),
                elements,
                sink_pad,
                frames,
                payload_type,
                _pipewire_fd: pipewire_fd,
                va_scaled,
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

    /// Whether `vapostproc` scales this chain.
    pub(crate) fn va_scaled(&self) -> bool {
        self.va_scaled
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
const MAX_BITRATE: u32 = 12_000_000;

/// What the phone's receiver reports say about the link, once a second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct LinkReport {
    /// 0..1.
    pub(crate) loss: f64,
    pub(crate) rtt_ms: Option<f64>,
    pub(crate) jitter_ms: Option<f64>,
}

/// Rate control on loss *and* delay: a queue building up (round trip above
/// the link's own baseline, or jitter) backs off before packets are lost.
/// Steps are small and smooth, except under real loss. `baseline_ms` is the
/// lowest round trip seen lately.
pub(crate) fn next_bitrate(current: u32, report: &LinkReport, baseline_ms: f64) -> u32 {
    let queueing = report
        .rtt_ms
        .is_some_and(|rtt| rtt > baseline_ms * 1.5 + 20.0)
        || report.jitter_ms.is_some_and(|jitter| jitter > 30.0);
    let factor = if report.loss > 0.10 {
        0.75
    } else if report.loss > 0.02 {
        0.92
    } else if queueing {
        0.90
    } else {
        1.05
    };
    ((f64::from(current) * factor) as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

/// Seconds of a condition before the frame rate changes, so it never flaps.
const FPS_HOLD_SECONDS: u32 = 5;

/// The second rung after bitrate: when even the lowest bitrate still loses
/// packets, fewer frames give each one more bits (text stays sharp, motion
/// gets choppier); when the link is clean again with room to spare, the full
/// rate (`full`, what the phone asked for) returns. Counts consecutive
/// seconds of each condition in `streak`.
pub(crate) fn next_fps(
    current: i32,
    full: i32,
    bitrate: u32,
    fraction_lost: f64,
    streak: &mut u32,
) -> i32 {
    let reduced = (full / 2).max(10).min(full);
    let wants_lower = current == full && bitrate == MIN_BITRATE && fraction_lost > 0.02;
    let wants_higher = current == reduced
        && current != full
        && bitrate >= START_BITRATE / 2
        && fraction_lost < 0.02;
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
        reduced
    } else {
        full
    }
}

/// Once a second, reads the phone's receiver reports from `webrtcbin` stats
/// and steers the encoder's bitrate, then the frame rate through
/// `videorate`'s `max-rate` (no caps pin the rate, so nothing has to
/// renegotiate). Debug builds log a stats line every 2 s. Holds only weak
/// references, so it ends with the pipeline; the returned flag ends it with
/// the capture.
fn spawn_bitrate_controller(
    webrtcbin: &gst::Element,
    encoder: &gst::Element,
    rate: &gst::Element,
    full_fps: i32,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let (webrtcbin, encoder, rate, flag) = (
        webrtcbin.downgrade(),
        encoder.downgrade(),
        rate.downgrade(),
        Arc::clone(&stop),
    );
    let spawned = std::thread::Builder::new()
        .name("desklink-screen-rate".to_string())
        .spawn(move || {
            let mut bitrate = START_BITRATE;
            let mut fps = full_fps;
            let mut streak = 0;
            let mut baseline_ms = f64::MAX;
            let mut tick = 0u64;
            let mut last_bytes = 0u64;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                tick += 1;
                let (Some(webrtcbin), Some(encoder)) = (webrtcbin.upgrade(), encoder.upgrade())
                else {
                    break;
                };
                let Some((report, bytes_sent)) = link_report(&webrtcbin) else {
                    continue;
                };
                if let Some(rtt) = report.rtt_ms {
                    // The baseline follows a link that got slower for good.
                    baseline_ms = (baseline_ms * 1.01).min(rtt);
                }
                let next = next_bitrate(bitrate, &report, baseline_ms);
                if next != bitrate {
                    bitrate = next;
                    set_encoder_bitrate(&encoder, bitrate);
                }
                let next = next_fps(fps, full_fps, bitrate, report.loss, &mut streak);
                if next != fps {
                    fps = next;
                    if let Some(rate) = rate.upgrade() {
                        set_if_present(&rate, "max-rate", &fps.to_string());
                    }
                }
                if cfg!(debug_assertions) && tick % 2 == 0 {
                    let kbps = bytes_sent.saturating_sub(last_bytes) * 8 / 2 / 1000;
                    debug_log!(
                        "[DeskLink] screen-stats: sent {kbps} kbps, target {} kbps, fps cap {fps}, loss {:.1}%, rtt {} ms, jitter {} ms",
                        bitrate / 1000,
                        report.loss * 100.0,
                        report.rtt_ms.map_or("?".to_string(), |rtt| format!("{rtt:.0}")),
                        report.jitter_ms.map_or("?".to_string(), |jitter| format!("{jitter:.0}"))
                    );
                }
                if tick % 2 == 0 {
                    last_bytes = bytes_sent;
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("[DeskLink] Screen bitrate control unavailable: {error}");
    }
    stop
}

/// The phone's receiver reports (the worst of them) and the bytes sent so
/// far, from `webrtcbin` stats.
fn link_report(webrtcbin: &gst::Element) -> Option<(LinkReport, u64)> {
    let promise = gst::Promise::new();
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    if promise.wait() != gst::PromiseResult::Replied {
        return None;
    }
    let reply = promise.get_reply()?;
    let mut report = LinkReport::default();
    let mut bytes = 0u64;
    let mut any = false;
    for stats in reply
        .iter()
        .filter_map(|(_, value)| value.get::<gst::Structure>().ok())
    {
        match stats.name().as_str() {
            "remote-inbound-rtp" => {
                any = true;
                if let Ok(lost) = stats.get::<f64>("fraction-lost") {
                    report.loss = report.loss.max(lost);
                }
                if let Ok(rtt) = stats.get::<f64>("round-trip-time") {
                    let ms = rtt * 1000.0;
                    report.rtt_ms = Some(report.rtt_ms.map_or(ms, |known: f64| known.max(ms)));
                }
                if let Ok(jitter) = stats.get::<f64>("jitter") {
                    let ms = jitter * 1000.0;
                    report.jitter_ms =
                        Some(report.jitter_ms.map_or(ms, |known: f64| known.max(ms)));
                }
            }
            "outbound-rtp" => {
                bytes += stats.get::<u64>("bytes-sent").unwrap_or(0);
            }
            _ => {}
        }
    }
    any.then_some((report, bytes))
}

fn set_if_present(element: &gst::Element, name: &str, value: &str) {
    if element.has_property(name, None) {
        element.set_property_from_str(name, value);
    }
}

/// Set once VA-API scaling failed in a real share: the probe below feeds it
/// system memory, while a share feeds it what PipeWire hands over (possibly
/// DMA-BUF), which can still fail. Every later chain this run then scales in
/// software.
static VA_SCALER_FAILED: AtomicBool = AtomicBool::new(false);

/// A share's VA-scaled chain errored or produced nothing: scale in software
/// from now on (the capture watchdog rebuilds the chain).
pub(crate) fn va_scaler_failed(why: &str) {
    if !VA_SCALER_FAILED.swap(true, Ordering::AcqRel) {
        eprintln!("[DeskLink] VA-API scaling failed in a share ({why}); using the software scaler");
    }
}

/// Whether VA-API post-processing can scale here: `vapostproc` exists and a
/// tiny test pipeline through it negotiates to system-memory I420 (it fails
/// on drivers without a usable video-processing entry point), and no share
/// has seen it fail. Probed once; `DESKLINK_SCREEN_SOFTWARE=1` forces the
/// software scaler.
fn va_scaler_works() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if VA_SCALER_FAILED.load(Ordering::Acquire) {
        return false;
    }
    *WORKS.get_or_init(|| {
        if std::env::var("DESKLINK_SCREEN_SOFTWARE").as_deref() == Ok("1")
            || gst::ElementFactory::find("vapostproc").is_none()
        {
            return false;
        }
        let Ok(pipeline) = gst::parse::launch(
            "videotestsrc num-buffers=2 ! video/x-raw,width=320,height=200 ! vapostproc ! \
             video/x-raw,format=NV12,width=160,height=100 ! videoconvert ! \
             video/x-raw,format=I420 ! fakesink",
        ) else {
            return false;
        };
        let worked = pipeline.set_state(gst::State::Playing).is_ok()
            && pipeline.bus().is_some_and(|bus| {
                bus.timed_pop_filtered(
                    gst::ClockTime::from_seconds(3),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .is_some_and(|message| matches!(message.view(), gst::MessageView::Eos(_)))
            });
        let _ = pipeline.set_state(gst::State::Null);
        worked
    })
}

/// Debug builds, `DESKLINK_SCREEN_H264=1`: send the desk's screen as VA-API
/// hardware H.264 instead of software VP8 — an experiment to measure against
/// VP8 (Phase 1c), off unless asked for, and only where VA can encode.
pub(crate) fn h264_experiment() -> bool {
    cfg!(debug_assertions)
        && std::env::var("DESKLINK_SCREEN_H264").as_deref() == Ok("1")
        && (gst::ElementFactory::find("vah264lpenc").is_some()
            || gst::ElementFactory::find("vah264enc").is_some())
        && gst::ElementFactory::find("rtph264pay").is_some()
}

/// Sets the encoder's bitrate, whichever unit it takes: VP8's
/// `target-bitrate` in bit/s, VA H.264's `bitrate` in kbit/s.
fn set_encoder_bitrate(encoder: &gst::Element, bits_per_second: u32) {
    if encoder.has_property("target-bitrate", None) {
        encoder.set_property("target-bitrate", bits_per_second as i32);
    } else if encoder.has_property("bitrate", None) {
        set_if_present(encoder, "bitrate", &(bits_per_second / 1000).to_string());
    }
}

/// Everything between a frame source and RTP: scale, convert, frame-rate
/// cap, leaky queue, low-latency VP8, payloader — in that order.
///
/// Sharp text first (Phase 1c): the picture is scaled with Lanczos (or VA-API
/// post-processing when the GPU has it) to the phone's own long side; the
/// rate cap only ever drops frames, so the last frame of a change goes out
/// at once; and VP8 is tuned the way remote-desktop products tune it for
/// screens — a quality floor, no noise filter, and refinement of a still
/// picture (the source resends it every 200 ms) instead of skipping it.
#[allow(clippy::type_complexity)]
fn encoder_chain(
    width: i32,
    height: i32,
    request: ScreenRequest,
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
    // `width`/`height` stay the compositor's logical geometry for pointer
    // mapping; only the encoded video is scaled, keeping the aspect ratio.
    let (capture_width, capture_height) = stream_dimensions(width, height, request.max_dimension)?;
    let fps = request.fps.clamp(1, MAX_FPS as i32);

    let make = |factory: &str, what: &str| {
        gst::ElementFactory::make(factory)
            .build()
            .map_err(|error| format!("DeskLink screen capture needs GStreamer {what}: {error}"))
    };
    let convert = make("videoconvert", "videoconvert")?;
    set_if_present(&convert, "n-threads", "4");
    let scale = if va_scaler_works() {
        let scale = make("vapostproc", "vapostproc")?;
        set_if_present(&scale, "scale-method", "hq");
        scale
    } else {
        let scale = make("videoscale", "videoscale")?;
        set_if_present(&scale, "method", "lanczos");
        set_if_present(&scale, "n-threads", "4");
        scale
    };
    // A cap, never a constant rate: drops frames above `fps`, never repeats
    // or holds one back (a held last frame of a change was up to 500 ms).
    let rate = make("videorate", "videorate")?;
    set_if_present(&rate, "drop-only", "true");
    set_if_present(&rate, "max-rate", &fps.to_string());
    set_if_present(&rate, "skip-to-first", "true");
    let filter = make("capsfilter", "capsfilter")?;
    // One frame, leaky: if the encoder falls behind, the *newest* frame
    // wins instead of a queue of stale ones adding latency.
    let queue = make("queue", "queue")?;
    queue.set_property("max-size-buffers", 1u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");
    if h264_experiment() {
        let encoder =
            make("vah264lpenc", "vah264lpenc").or_else(|_| make("vah264enc", "vah264enc"))?;
        let payloader = make("rtph264pay", "rtph264pay")?;
        filter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "NV12")
                .field("width", capture_width)
                .field("height", capture_height)
                .build(),
        );
        // Low-latency hardware H.264 for the phone's hardware decoder: CBR,
        // no B-frames, one reference, a small coded-picture buffer.
        set_if_present(&encoder, "rate-control", "cbr");
        set_if_present(&encoder, "b-frames", "0");
        set_if_present(&encoder, "ref-frames", "1");
        set_if_present(&encoder, "target-usage", "7");
        set_if_present(&encoder, "key-int-max", &(fps * 10).to_string());
        set_encoder_bitrate(&encoder, START_BITRATE);
        set_if_present(
            &encoder,
            "cpb-size",
            &(START_BITRATE / 1000 / 4).to_string(),
        );
        set_if_present(&payloader, "aggregate-mode", "zero-latency");
        set_if_present(&payloader, "config-interval", "-1");
        debug_log!("[DeskLink] Screen experiment: VA-API H.264 to the phone");
        return Ok((scale, convert, rate, filter, queue, encoder, payloader));
    }
    let encoder = make("vp8enc", "vp8enc")?;
    let payloader = make("rtpvp8pay", "rtpvp8pay")?;

    // Size and format only: a pinned frame rate is what made the adaptive
    // frame rate fail to renegotiate.
    filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", capture_width)
            .field("height", capture_height)
            .build(),
    );
    encoder.set_property("deadline", 1i64);
    encoder.set_property("cpu-used", 5i32);
    encoder.set_property("target-bitrate", START_BITRATE as i32);
    // Keyframes come on the decoder's request (throttled); this is only the
    // safety net for a request that never arrives.
    encoder.set_property("keyframe-max-dist", fps * 10);
    // Low-latency rate control: libvpx defaults buffer 4–6 s of data,
    // which lets bursts queue instead of shrinking quality.
    set_if_present(&encoder, "threads", "4");
    set_if_present(&encoder, "buffer-size", "150");
    set_if_present(&encoder, "buffer-initial-size", "100");
    set_if_present(&encoder, "buffer-optimal-size", "120");
    set_if_present(&encoder, "end-usage", "cbr");
    set_if_present(&encoder, "error-resilient", "default");
    // Screen content: a quality floor so text never smears, a ceiling high
    // enough that a large change goes out fast, keyframes kept from
    // bursting, and no noise filter (it blurs glyphs). Static blocks are
    // encoded, not skipped, so a still picture keeps sharpening.
    set_if_present(&encoder, "min-quantizer", "10");
    set_if_present(&encoder, "max-quantizer", "50");
    set_if_present(&encoder, "max-intra-bitrate", "300");
    set_if_present(&encoder, "noise-sensitivity", "0");
    set_if_present(&encoder, "static-threshold", "0");
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
        encoder_chain(source.width, source.height, ScreenRequest::FALLBACK)?;
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
    // A still screen is resent every 200 ms, so the encoder keeps refining
    // it (top-off) rather than leaving the first, coarse version up.
    set_if_present(&source, "keepalive-time", "200");
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
fn negotiated_payload(webrtcbin: &gst::Element, wanted: &str) -> Option<u32> {
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
                .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
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
    /// What the desk serves when the phone asked nothing (older phones):
    /// 1280 px, 30 fps.
    pub(crate) const FALLBACK: Self = Self {
        max_dimension: 1280,
        fps: 30,
    };

    /// The phone's `maxDimension` (its real landscape long side) and `fps`,
    /// each when given, clamped to what this desk serves (contract V1).
    pub(crate) fn from_parts(max_dimension: Option<i64>, fps: Option<i64>) -> Self {
        Self {
            max_dimension: max_dimension
                .unwrap_or(i64::from(Self::FALLBACK.max_dimension))
                .clamp(MIN_DIMENSION, MAX_DIMENSION) as i32,
            fps: fps
                .unwrap_or(i64::from(Self::FALLBACK.fps))
                .clamp(1, MAX_FPS) as i32,
        }
    }

    /// What is served for a screen `width`×`height`: software VP8 cannot
    /// keep up with 60 fps above 1920 px, so a larger picture runs at 30.
    pub(crate) fn served(self, width: i32, height: i32) -> Self {
        let long_side = self.max_dimension.min(width.max(height));
        Self {
            max_dimension: self.max_dimension,
            fps: if long_side > 1920 {
                self.fps.min(30)
            } else {
                self.fps
            },
        }
    }

    /// Reads a request body, clamping to what this desktop will actually serve.
    ///
    /// The values arrive from the paired device, so they are bounds-checked
    /// rather than trusted: an unclamped `fps` or dimension would size a real
    /// encoder pipeline from a remote number.
    pub fn from_body(packet: &crate::device_links::packet::NetworkPacket) -> Self {
        Self::from_parts(packet.get_i64("maxDimension"), packet.get_i64("fps"))
    }
}

impl Default for ScreenRequest {
    fn default() -> Self {
        Self::FALLBACK
    }
}

/// The encoded picture's long side, at least and at most (contract V1).
const MIN_DIMENSION: i64 = 640;
const MAX_DIMENSION: i64 = 2560;
/// The highest frame rate served.
const MAX_FPS: i64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_follows_loss_and_queueing_in_small_steps() {
        let clean = LinkReport {
            loss: 0.0,
            rtt_ms: Some(20.0),
            jitter_ms: Some(2.0),
        };
        let up = next_bitrate(START_BITRATE, &clean, 20.0);
        assert!(
            up > START_BITRATE && up <= START_BITRATE / 100 * 106,
            "a small probe up"
        );
        let queueing = LinkReport {
            rtt_ms: Some(120.0),
            ..clean
        };
        assert!(
            next_bitrate(START_BITRATE, &queueing, 20.0) < START_BITRATE,
            "delay backs off before loss"
        );
        let jittery = LinkReport {
            jitter_ms: Some(45.0),
            ..clean
        };
        assert!(next_bitrate(START_BITRATE, &jittery, 20.0) < START_BITRATE);
        let light_loss = LinkReport {
            loss: 0.05,
            ..clean
        };
        let heavy_loss = LinkReport {
            loss: 0.20,
            ..clean
        };
        let light = next_bitrate(START_BITRATE, &light_loss, 20.0);
        let heavy = next_bitrate(START_BITRATE, &heavy_loss, 20.0);
        assert!(
            heavy < light && light < START_BITRATE,
            "harder under real loss"
        );
        assert_eq!(next_bitrate(MIN_BITRATE, &heavy_loss, 20.0), MIN_BITRATE);
        assert_eq!(next_bitrate(MAX_BITRATE, &clean, 20.0), MAX_BITRATE);
        let floor = (0..20).fold(START_BITRATE, |rate, _| {
            next_bitrate(rate, &heavy_loss, 20.0)
        });
        assert_eq!(floor, MIN_BITRATE);
    }

    #[test]
    fn frame_rate_drops_only_after_sustained_loss_at_the_floor_and_recovers() {
        let full = 60;
        let mut streak = 0;
        // Loss above the floor is the bitrate's job, not the frame rate's.
        for _ in 0..10 {
            assert_eq!(next_fps(full, full, START_BITRATE, 0.3, &mut streak), full);
        }
        let mut fps = full;
        for second in 1..=FPS_HOLD_SECONDS {
            fps = next_fps(fps, full, MIN_BITRATE, 0.3, &mut streak);
            assert_eq!(fps, if second < FPS_HOLD_SECONDS { full } else { 30 });
        }
        // A clean second in between resets the count: no flapping.
        assert_eq!(next_fps(fps, full, START_BITRATE, 0.0, &mut streak), 30);
        assert_eq!(next_fps(fps, full, START_BITRATE, 0.3, &mut streak), 30);
        for _ in 0..FPS_HOLD_SECONDS {
            fps = next_fps(fps, full, START_BITRATE, 0.0, &mut streak);
        }
        assert_eq!(fps, full);
        // Already slow: nothing lower than 10.
        assert_eq!(next_fps(10, 10, MIN_BITRATE, 0.5, &mut 99), 10);
    }

    /// Sharp and prompt: Lanczos (or VA) scaling to the phone's size, a frame
    /// rate cap that only drops, caps that do not pin the rate, and VP8 tuned
    /// for screens.
    /// Once a share's VA scaler failed, every later chain scales in
    /// software.
    #[test]
    fn a_failed_va_scaler_is_not_used_again() {
        if gst::init().is_err() {
            return;
        }
        va_scaler_failed("test");
        assert!(!va_scaler_works());
        let request = ScreenRequest::from_parts(Some(1280), Some(30)).served(1920, 1080);
        let (scale, ..) = encoder_chain(1920, 1080, request).unwrap();
        assert_eq!(
            scale
                .factory()
                .map(|factory| factory.name().to_string())
                .as_deref(),
            Some("videoscale")
        );
    }

    #[test]
    fn the_encoder_chain_is_tuned_for_screens() {
        gst::init().unwrap();
        let request = ScreenRequest::from_parts(Some(2400), Some(60)).served(2560, 1600);
        assert_eq!(request.fps, 30, "software VP8 above 1920 px runs at 30");
        let (scale, convert, rate, filter, _queue, encoder, _payloader) =
            encoder_chain(2560, 1600, request).unwrap();
        match scale
            .factory()
            .map(|factory| factory.name().to_string())
            .as_deref()
        {
            Some("videoscale") => {
                assert_eq!(scale.property::<u32>("n-threads"), 4);
                let method = scale.property_value("method");
                assert_eq!(
                    gst::glib::EnumValue::from_value(&method).map(|(_, value)| value.nick()),
                    Some("lanczos")
                );
            }
            Some("vapostproc") => {}
            other => panic!("unexpected scaler {other:?}"),
        }
        assert!(convert.has_property("n-threads", None));
        assert!(rate.property::<bool>("drop-only"));
        assert_eq!(rate.property::<i32>("max-rate"), 30);
        let caps = filter.property::<gst::Caps>("caps");
        let structure = caps.structure(0).unwrap();
        assert!(
            !structure.has_field("framerate"),
            "the rate is never pinned"
        );
        assert_eq!(structure.get::<i32>("width").unwrap(), 2400);
        assert_eq!(structure.get::<i32>("height").unwrap(), 1500);
        assert_eq!(encoder.property::<i32>("min-quantizer"), 10);
        assert_eq!(encoder.property::<i32>("max-quantizer"), 50);
        assert_eq!(encoder.property::<i32>("noise-sensitivity"), 0);
        assert_eq!(encoder.property::<i32>("static-threshold"), 0);
        assert_eq!(encoder.property::<i32>("cpu-used"), 5);
        // A phone that fits in 1920 gets its 60 fps.
        assert_eq!(
            ScreenRequest::from_parts(Some(1920), Some(60))
                .served(2560, 1600)
                .fps,
            60
        );
    }

    use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SCREEN_REQUEST};

    #[test]
    fn screen_request_clamps_values_supplied_by_the_peer() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
        packet.set("maxDimension", 99_999);
        packet.set("fps", 240);

        let request = ScreenRequest::from_body(&packet);

        assert_eq!(request.max_dimension, 2560);
        assert_eq!(request.fps, 60);
        let small = ScreenRequest::from_parts(Some(100), Some(0));
        assert_eq!((small.max_dimension, small.fps), (640, 1));
    }

    #[test]
    fn screen_request_falls_back_to_defaults() {
        let packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
        assert_eq!(ScreenRequest::from_body(&packet), ScreenRequest::FALLBACK);
        assert_eq!(
            ScreenRequest::from_parts(None, None),
            ScreenRequest::FALLBACK
        );
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
        assert_eq!(stream_dimensions(2560, 1600, 2400).unwrap(), (2400, 1500));
        assert_eq!(stream_dimensions(2560, 1600, 9999).unwrap(), (2560, 1600));
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
