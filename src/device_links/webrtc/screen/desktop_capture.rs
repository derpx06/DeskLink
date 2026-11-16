//! This desktop's screen going to the phone: [`DesktopScreenCapture`] (start,
//! attach to the negotiated section, stop until idle), RTP continuity across
//! shares, and the debug self-test. It reads the chain's statistics from a
//! `ChainStats` (owned by `bitrate`).
//!
//! Sibling edges: `encoder` (the chain and sources), `bitrate` (the
//! controller and chain statistics). Outside: `remote_desktop_portal`, `config`
//! (the remembered grant).

use super::*;

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
    #[cfg_attr(not(test), allow(dead_code))]
    chain_stats: ChainStats,
}

/// Where a desktop share's frames come from.
pub(crate) enum DesktopSource {
    /// A portal-approved PipeWire stream.
    PipeWire {
        fd: OwnedFd,
        node_id: u32,
        serial: Option<u64>,
        /// An extended display (F2): the pixel size pinned on PipeWire,
        /// which is what creates the virtual monitor at that size.
        virtual_size: Option<(i32, i32)>,
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
        let (source, pipewire_fd, pinned) = match source {
            DesktopSource::PipeWire { node_id: 0, .. } => {
                return Err(
                    "The desktop portal returned invalid screen stream metadata".to_string()
                );
            }
            DesktopSource::PipeWire {
                fd,
                node_id,
                serial,
                virtual_size,
            } => (
                pipewire_source(&fd, node_id, serial)?,
                Some(fd),
                virtual_size,
            ),
            DesktopSource::X11 => (x11_source()?, None, None),
        };
        Self::attach_source_sized(
            pipeline,
            webrtcbin,
            source,
            pipewire_fd,
            (width, height),
            request,
            continuity,
            pinned,
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
        Self::attach_source_sized(
            pipeline,
            webrtcbin,
            source,
            pipewire_fd,
            (width, height),
            request,
            continuity,
            None,
        )
    }

    /// [`Self::attach_source`], with the source's size `pinned` on its caps:
    /// an extended display's virtual monitor takes the size its consumer
    /// negotiates (unpinned it came out 1×1).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_source_sized(
        pipeline: &gst::Pipeline,
        webrtcbin: &gst::Element,
        source: gst::Element,
        pipewire_fd: Option<OwnedFd>,
        (width, height): (i32, i32),
        request: ScreenRequest,
        continuity: Option<RtpContinuity>,
        pinned: Option<(i32, i32)>,
    ) -> Result<Self, String> {
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return Err("The desktop portal returned invalid screen stream metadata".to_string());
        }
        let request = request.served(width, height);
        let (scale, convert, rate, filter, queue, encoder, payloader) =
            encoder_chain(width, height, request)?;

        // Frames in system memory. Offered DMA-BUF (vapostproc takes it),
        // PipeWire negotiated buffers with Mutter's tiled modifiers that
        // the GPU path then refused: not-negotiated on the first frame, and
        // every reconnect to that stream failed (`gst_caps_is_fixed`
        // assertion, "error use input buffers: -22") until the share ended.
        let caps = match pinned {
            Some((w, h)) => gst::Caps::builder("video/x-raw")
                .field("width", w)
                .field("height", h)
                .field(
                    "max-framerate",
                    gst::Fraction::new(request.fps.clamp(1, MAX_FPS as i32), 1),
                )
                .build(),
            None => gst::Caps::new_empty_simple("video/x-raw"),
        };
        let system_memory = gst::ElementFactory::make("capsfilter")
            .property("caps", caps)
            .build()
            .map_err(|error| format!("DeskLink screen capture needs capsfilter: {error}"))?;
        // Scale before converting: colour conversion then runs on the small
        // frame, not the full-resolution one.
        let mut elements = vec![source, system_memory];
        // The capture says how its picture must be turned (PipeWire's video
        // transform, which `pipewiresrc` turns into an `image-orientation`
        // tag); nothing downstream read it, so a rotated or flipped output was
        // encoded as it came and the phone showed it upside down.
        elements.extend(orientation_filter());
        elements.extend([
            scale,
            convert,
            rate.clone(),
            filter,
            queue.clone(),
            encoder.clone(),
            payloader.clone(),
        ]);
        pipeline
            .add_many(elements.iter())
            .map_err(|error| format!("Could not add DeskLink screen encoder chain: {error}"))?;
        let frames = Arc::new(AtomicU64::new(0));
        let chain_stats = ChainStats::default();
        let queue_sink = queue.static_pad("sink");
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
                    section_pad(webrtcbin, mline)?
                }
            };
            // The sink pad is kept on failure, never released: requesting it
            // again made webrtcbin build a second FEC/RED encoder for the
            // transceiver ("duplicate call to create a fec encoder"), and
            // every later share on that peer died not-linked. Removing the
            // chain unlinks it; the next share finds it unlinked and reuses it.
            let fail = |message: String| Err(message);
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
                payloader.set_property("timestamp-offset", continuity.timestamp_offset);
                if let Some(resume) = continuity.resume {
                    payloader.set_property("seqnum-offset", i32::from(resume.seqnum));
                    if let Some(id) = resume.picture_id {
                        if payloader.has_property("picture-id-offset", None) {
                            payloader.set_property("picture-id-offset", i32::from(id));
                        }
                    }
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
            if crate::debug_logging() {
                chain_stats.watch(&queue_sink, &encoder);
            }
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
                    chain_stats.clone(),
                    continuity.map(|continuity| continuity.ssrc),
                ),
                chain_stats,
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

    /// Where the next share's stream must carry on, once this one has sent
    /// anything. Read after [`Self::detach`]: a packet numbered later can
    /// no longer reach the wire, so the next share never repeats one.
    pub(crate) fn resume(&self) -> Option<RtpResume> {
        if self.frames_sent() == 0 {
            return None;
        }
        let payloader = self
            .elements
            .last()
            .filter(|payloader| payloader.has_property("seqnum", None))?;
        Some(RtpResume {
            seqnum: (payloader.property::<u32>("seqnum") as u16).wrapping_add(1),
            // One past the id the payloader would give its next frame: a
            // frame still being pushed when the chain was detached may have
            // gone out with that id already, and libwebrtc drops a repeat.
            // A gap of one is harmless.
            picture_id: payloader
                .has_property("picture-id", None)
                .then(|| ((payloader.property::<i32>("picture-id") + 1) & 0x7fff) as u16),
        })
    }

    /// Unlinks the chain from `webrtcbin`: nothing more of it reaches the
    /// wire. The pad is deliberately *kept*, not released: `webrtcbin` builds
    /// the sender chain (including its FEC/RED encoders) when the pad is
    /// requested, and asking for the same section's pad again a second time
    /// logs a CRITICAL and leaves that chain half-built. The next share finds
    /// this pad again.
    pub(crate) fn detach(&self) {
        if let Some(payloader_src) = self
            .elements
            .last()
            .and_then(|element| element.static_pad("src"))
        {
            let _ = payloader_src.unlink(&self.sink_pad);
        }
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

    /// Key-unit requests that reached the encoder, and keyframes it made.
    #[cfg(test)]
    pub(crate) fn key_counts(&self) -> (u64, u64) {
        (
            self.chain_stats.key_requests.load(Ordering::Relaxed),
            self.chain_stats.keyframes.load(Ordering::Relaxed),
        )
    }

    /// The encoder's target bitrate (bit/s), as the rate controller last set it.
    #[cfg(test)]
    pub(crate) fn encoder_bitrate(&self) -> Option<i32> {
        self.elements
            .iter()
            .find(|element| element.has_property("target-bitrate", None))
            .map(|encoder| encoder.property::<i32>("target-bitrate"))
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
        // Unlink from `webrtcbin` first so nothing new can enter it.
        self.detach();
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
/// connection: same SSRC, and sequence numbers, timestamps and VP8 picture
/// ids all carrying on. The phone binds its picture to the first SSRC it
/// sees, and SRTP refuses to encrypt a sequence number it has already used
/// for that SSRC ("general supporting library error" in srtpenc). And to
/// the phone it is one stream: libwebrtc numbers VP8 frames by picture id
/// and drops a frame numbered at or before one it already decoded (a
/// keyframe is let through only with a newer RTP timestamp). Each new
/// payloader started at a random picture id and timestamp, so about half of
/// the reopened views dropped every frame, keyframes and the answers to the
/// phone's PLIs included: "waiting for the first picture" forever (F8).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RtpContinuity {
    pub ssrc: u32,
    /// The connection's RTP clock: a payloader's timestamps are this plus the
    /// running time, so a later share's are ahead of an earlier one's.
    pub timestamp_offset: u32,
    /// Where the previous share stopped; `None` for the first.
    pub resume: Option<RtpResume>,
}

/// The next sequence number and VP8 picture id after a share's last packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RtpResume {
    pub seqnum: u16,
    /// `None` for a payloader without picture ids (the H.264 experiment).
    pub picture_id: Option<u16>,
}

/// Longest [`DesktopScreenCapture::stop`] waits for the chain to go idle.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// VP8's payload type when the peer's offer does not make one visible.
const DEFAULT_VP8_PAYLOAD: u32 = 96;

/// Debug builds: the real desktop-share path without a phone — GNOME portal
/// (resuming `device_id`'s remembered grant, or asking at the desktop),
/// PipeWire capture of this screen, the exact encoder chain the phone gets,
/// and one pointer nudge through the portal. Blocks for a few seconds.
#[cfg(debug_assertions)]
pub(crate) fn desktop_share_selftest(device_id: &str) -> Result<String, String> {
    use crate::device_links::webrtc::remote_desktop_portal::{DesktopSession, RemoteDesktopPortal};
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
    let system_memory = gst::ElementFactory::make("capsfilter")
        .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
        .build()
        .map_err(|error| error.to_string())?;
    let chain = [
        capture,
        system_memory,
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
