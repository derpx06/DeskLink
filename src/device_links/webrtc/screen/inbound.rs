//! The phone's screen arriving here: the decoded [`ScreenStream`] the window
//! shows, low-latency decoder choice, parking a pad while no window shows it,
//! the per-device stream registry, and the inbound statistics and watch.
//!
//! Sibling edges: none (the parent's caps and `set_if_present`).

use super::*;

/// The rendering tail of an inbound screen stream.
///
/// The sink element is held rather than the paintable itself: a
/// `gdk::Paintable` is not `Send`, so it cannot travel from the GStreamer
/// thread that builds this pipeline to the GTK thread that displays it. A
/// `gst::Element` can, and the paintable is read from it on the main thread.
#[derive(Clone)]
pub struct ScreenStream {
    pub(super) inner: Arc<ScreenStreamInner>,
}

pub(super) struct ScreenStreamInner {
    pipeline: gst::Pipeline,
    source_pad: gst::Pad,
    pub(super) elements: Vec<gst::Element>,
    sink: gst::Element,
    stopped: AtomicBool,
    /// Decoded frames that reached the sink.
    shown: Arc<AtomicU64>,
    /// When the last decoded frame entered the sink (`DESKLINK_DEBUG` only).
    last_frame: Mutex<Option<std::time::Instant>>,
}

impl ScreenStreamInner {
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        // Held while it changes hands: RTP into an unlinked pad stops the
        // whole receive path (see `while_blocked`).
        while_blocked(&self.source_pad, || {
            if let Some(decode_sink) = self
                .elements
                .first()
                .and_then(|decode| decode.static_pad("sink"))
            {
                let _ = self.source_pad.unlink(&decode_sink);
            }
            park_pad(&self.pipeline, &self.source_pad);
            for element in self.elements.iter().rev() {
                let _ = element.set_state(gst::State::Null);
            }
            let _ = self.pipeline.remove_many(self.elements.iter());
        });
    }
}

/// Whether `factory` is a decoder that holds every frame of a phone's H.264
/// stream until the *next* frame arrives. GStreamer's VA-API H.264 decoder
/// (`vah264dec`, 1.28) does so for streams with `pic_order_cnt_type` 2, which
/// is what libwebrtc's and the phone's hardware encoders send: measured, a
/// 5 fps stream came out 197 ms per frame through `vah264dec` (compliance
/// auto, normal or flexible alike) against 2 ms through `avdec_h264`. Live,
/// it showed as "after jitter buffer" equal to one frame interval at every
/// rate (25 ms at 40 fps, 100 ms at 10 fps, seconds on a still screen): the
/// last frame of every change, and every refined still, waited for the next.
fn holds_a_frame(factory: &str) -> bool {
    factory.starts_with("va") && factory.contains("h264") && factory.ends_with("dec")
}

/// `decodebin`'s candidate decoders reordered so that one that holds a frame
/// ([`holds_a_frame`]) is tried only when nothing else can decode; `None`
/// when the order stays as it is.
pub(super) fn low_latency_order(factories: &[String]) -> Option<Vec<usize>> {
    let (mut order, held): (Vec<usize>, Vec<usize>) =
        (0..factories.len()).partition(|&index| !holds_a_frame(&factories[index]));
    if held.is_empty() || order.is_empty() {
        return None;
    }
    order.extend(held);
    Some(order)
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
pub(crate) fn park_pad(pipeline: &gst::Pipeline, pad: &gst::Pad) {
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

/// Runs `change` while `pad` holds its data: an inbound RTP pad must never
/// be found unlinked, even for a moment, or webrtcbin's shared receive path
/// stops with `not-linked` (data channels included). Measured: a session
/// switch with audio flowing hit that window every time.
pub(crate) fn while_blocked<R>(pad: &gst::Pad, change: impl FnOnce() -> R) -> R {
    let probe = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
        gst::PadProbeReturn::Ok
    });
    let result = change();
    if let Some(probe) = probe {
        pad.remove_probe(probe);
    }
    result
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
    let attached = while_blocked(&pad, || {
        if let Some(peer) = pad.peer() {
            let _ = pad.unlink(&peer);
            if let Some(fake) = peer.parent_element() {
                let _ = fake.set_state(gst::State::Null);
                let _ = pipeline.remove(&fake);
            }
        }
        let attached = attach_incoming_video(&pipeline, &pad);
        if attached.is_err() {
            park_pad(&pipeline, &pad);
        }
        attached
    });
    match attached {
        Ok(stream) => {
            // The phone's encoder carries on from the first view and sends
            // delta frames the new decoder cannot start from. Asked now, the
            // picture comes with the next frame; left to the decoder it took
            // 0.2-0.5 s more on the emulator (a fresh decoder asks only once
            // it has failed to decode). One request per reshare.
            stream.request_keyframe();
            publish_stream(device_id, stream);
            true
        }
        Err(error) => {
            eprintln!("[DeskLink] Could not show the phone's screen again: {error}");
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

    /// Asks the sender for a keyframe the way a decoder does (an upstream
    /// `GstForceKeyUnit`, which the RTP session turns into a PLI).
    pub(crate) fn request_keyframe(&self) -> bool {
        self.inner
            .source_pad
            .send_event(gst::event::CustomUpstream::new(
                gst::Structure::builder("GstForceKeyUnit")
                    .field("all-headers", true)
                    .build(),
            ))
    }

    /// How long ago the last decoded frame entered the display sink
    /// (`DESKLINK_DEBUG` only; `None` otherwise).
    pub fn since_last_frame(&self) -> Option<std::time::Duration> {
        self.inner.last_frame.lock().ok()?.map(|at| at.elapsed())
    }

    /// Decoded frames delivered to the display so far.
    pub fn frames_shown(&self) -> u64 {
        self.inner.shown.load(Ordering::Relaxed)
    }

    /// Where decoded frames enter the display sink (a fakesink in tests).
    #[cfg(test)]
    pub(crate) fn sink_pad(&self) -> Option<gst::Pad> {
        self.inner.sink.static_pad("sink")
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
pub(super) fn streams() -> &'static Mutex<std::collections::HashMap<String, ScreenStream>> {
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
    // A software decoder's frames are converted here, every frame on the path to the screen.
    set_if_present(&convert, "n-threads", "4");
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
        //
        // The decoder asks on the jitterbuffer's packet-lost events (a
        // missing-data gap from the depayloader), not on corrupt output, so
        // those must mean real loss: see `retransmit_by_media_section` (F24).
        let decode_bin = decode
            .downcast_ref::<gst::Bin>()
            .ok_or_else(|| "DeskLink screen decoder is not a bin".to_string())?;
        decode.connect("autoplug-sort", false, |values| {
            let factories = values.get(3)?.get::<gst::glib::ValueArray>().ok()?;
            let names: Vec<String> = factories
                .iter()
                .map(|value| {
                    value
                        .get::<gst::ElementFactory>()
                        .map(|factory| factory.name().to_string())
                        .unwrap_or_default()
                })
                .collect();
            let sorted = low_latency_order(&names).map(|order| {
                gst::glib::ValueArray::from_values(order.into_iter().map(|i| factories[i].clone()))
            });
            Some(sorted.to_value())
        });
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
        last_frame: Mutex::new(None),
    });
    let weak_inner = Arc::downgrade(&inner);
    pad.connect_unlinked(move |_, _| {
        if let Some(inner) = weak_inner.upgrade() {
            inner.stop();
        }
    });

    if crate::debug_logging() {
        watch_inbound(pad, &inner);
    }
    Ok(ScreenStream { inner })
}

/// What this desk has received from the phone's video stream so far.
#[derive(Debug, PartialEq)]
pub(crate) struct Inbound {
    pub(crate) bytes: u64,
    pub(crate) lost: i64,
    pub(crate) nacks: u32,
    pub(crate) plis: u32,
    pub(crate) jitter_ms: Option<f64>,
}

impl Inbound {
    /// Counted from `start` instead of the session's beginning (bytes stay
    /// absolute: they are differenced line by line).
    pub(crate) fn since(&self, start: &Inbound) -> Inbound {
        Inbound {
            bytes: self.bytes,
            lost: self.lost - start.lost,
            nacks: self.nacks.saturating_sub(start.nacks),
            plis: self.plis.saturating_sub(start.plis),
            jitter_ms: self.jitter_ms,
        }
    }
}

/// The phone's video source among rtpbin's `source-stats`: the remote
/// sender that has delivered the most bytes (the audio and the RTCP-only
/// sources deliver far fewer).
pub(super) fn inbound_summary(sources: impl Iterator<Item = gst::Structure>) -> Option<Inbound> {
    let source = sources
        .filter(|source| {
            !source.get::<bool>("internal").unwrap_or(true)
                && source.get::<bool>("is-sender").unwrap_or(false)
        })
        .max_by_key(|source| source.get::<u64>("octets-received").unwrap_or(0))?;
    let clock_rate = source
        .get::<i32>("clock-rate")
        .ok()
        .filter(|rate| *rate > 0)
        .unwrap_or(90_000);
    Some(Inbound {
        bytes: source.get::<u64>("octets-received").unwrap_or(0),
        lost: source.get::<i32>("packets-lost").map_or(0, i64::from),
        nacks: source.get::<u32>("sent-nack-count").unwrap_or(0),
        plis: source.get::<u32>("sent-pli-count").unwrap_or(0),
        jitter_ms: source
            .get::<u32>("jitter")
            .ok()
            .map(|jitter| f64::from(jitter) * 1000.0 / f64::from(clock_rate)),
    })
}

/// The phone's latest RTCP sender report, from the same source as
/// [`inbound_summary`]: its NTP wall-clock time (32.32) and the RTP timestamp
/// of that instant. Maps a frame's RTP timestamp back to its capture time.
pub(super) fn sender_report(sources: impl Iterator<Item = gst::Structure>) -> Option<(u64, u32)> {
    let source = sources
        .filter(|source| {
            !source.get::<bool>("internal").unwrap_or(true)
                && source.get::<bool>("is-sender").unwrap_or(false)
        })
        .max_by_key(|source| source.get::<u64>("octets-received").unwrap_or(0))?;
    source
        .get::<bool>("have-sr")
        .unwrap_or(false)
        .then(|| {
            Some((
                source.get::<u64>("sr-ntptime").ok()?,
                source.get::<u32>("sr-rtptime").ok()?,
            ))
        })
        .flatten()
}

/// Milliseconds from a frame's capture on the phone to `shown_unix_ns` here:
/// the capture instant is the sender report's NTP time moved by the RTP
/// clock (90 kHz) to the frame's timestamp. Phone clock against this one:
/// the difference between the two clocks is still in it.
pub(super) fn capture_to_shown_ms(report: (u64, u32), rtp: u32, shown_unix_ns: i128) -> f64 {
    const NTP_TO_UNIX_SECONDS: i128 = 2_208_988_800;
    let (ntp, report_rtp) = report;
    let report_ns = ((ntp >> 32) as i128 - NTP_TO_UNIX_SECONDS) * 1_000_000_000
        + (((ntp & 0xffff_ffff) as i128 * 1_000_000_000) >> 32);
    let ticks = rtp.wrapping_sub(report_rtp) as i32;
    let capture_ns = report_ns + i128::from(ticks) * 1_000_000_000 / 90_000;
    (shown_unix_ns - capture_ns) as f64 / 1e6
}

/// `DESKLINK_FRAME_DIR`, the measurement harness's folder, read once: frames
/// of the phone's screen are written there, so only a real directory of this
/// user's that nobody else can write is taken.
pub(crate) fn frame_dir() -> Option<&'static std::path::Path> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("DESKLINK_FRAME_DIR")?);
        if private_dir(&dir) {
            Some(dir)
        } else {
            eprintln!(
                "[DeskLink] DESKLINK_FRAME_DIR ignored: {} is not a folder of this user's that only it can write",
                dir.display()
            );
            None
        }
    })
    .as_deref()
}

/// A directory itself (not a link to one), owned by this user, with no group
/// or other write permission.
pub(super) fn private_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == unsafe { libc::getuid() } && meta.mode() & 0o022 == 0
    })
}

/// Opens `name` in `dir` for this user only (0600), never through a link at
/// that name: appended to, or emptied first.
pub(crate) fn open_private(
    dir: &std::path::Path,
    name: &str,
    append: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(name))
}

/// The per-frame record stops growing here (about 300 000 frames).
const FRAMES_CSV_MAX_BYTES: u64 = 8 << 20;

fn unix_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as i128)
}

/// The RTP timestamp of an RTP packet (bytes 4-7 of its header).
fn rtp_timestamp(buffer: &gst::BufferRef) -> Option<u32> {
    let map = buffer.map_readable().ok()?;
    let bytes: [u8; 4] = map.get(4..8)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// UDP datagrams the kernel dropped because a socket's receive buffer was
/// full, IPv4 and IPv6, every socket on this machine (`/proc/net/snmp{,6}`).
pub(super) fn udp_receive_buffer_drops() -> Option<u64> {
    let snmp = std::fs::read_to_string("/proc/net/snmp").ok()?;
    let mut udp = snmp.lines().filter(|line| line.starts_with("Udp: "));
    let (names, values) = (udp.next()?, udp.next()?);
    let index = names
        .split_whitespace()
        .position(|name| name == "RcvbufErrors")?;
    let v4 = values.split_whitespace().nth(index)?.parse::<u64>().ok()?;
    let v6 = std::fs::read_to_string("/proc/net/snmp6")
        .ok()
        .and_then(|snmp6| {
            snmp6.lines().find_map(|line| {
                line.strip_prefix("Udp6RcvbufErrors")?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
        })
        .unwrap_or(0);
    Some(v4 + v6)
}

/// `DESKLINK_DEBUG`: a `phone-screen-stats` line every 2 s while a phone's
/// screen is shown — what arrives (size, frames shown, kbps, loss, NACK/PLI
/// sent, jitter) and what this desk adds after the jitter buffer: depayload,
/// decode and conversion per frame, matched by timestamp.
fn watch_inbound(pad: &gst::Pad, inner: &Arc<ScreenStreamInner>) {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    let Some(webrtcbin) = pad.parent_element() else {
        return;
    };
    // pts -> (arrival of the frame's last packet, its RTP timestamp).
    type Arrivals = HashMap<u64, (Instant, Option<u32>)>;
    let arrived: Arc<Mutex<Arrivals>> = Arc::default();
    // (sum ms, frames, max ms) since the last line.
    let chain: Arc<Mutex<(f64, u32, f64)>> = Arc::default();
    // (RTP timestamp, wall clock ns) of each frame that reached the sink since the last line.
    let shown_at: Arc<Mutex<Vec<(u32, i128)>>> = Arc::default();
    let last_buffer = |data: &Option<gst::PadProbeData>| match data {
        Some(gst::PadProbeData::Buffer(buffer)) => Some(buffer.to_owned()),
        Some(gst::PadProbeData::BufferList(list)) => list.iter().last().map(|b| b.to_owned()),
        _ => None,
    };
    {
        let arrived = Arc::clone(&arrived);
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, info| {
                if let Some(buffer) = last_buffer(&info.data) {
                    if let Some(pts) = buffer.pts() {
                        let mut arrived = arrived.lock().unwrap_or_else(|e| e.into_inner());
                        if arrived.len() > 512 {
                            arrived.clear();
                        }
                        // The frame's last packet: decoding can start from here.
                        arrived.insert(pts.nseconds(), (Instant::now(), rtp_timestamp(&buffer)));
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
    }
    if let Some(sink_pad) = inner.sink.static_pad("sink") {
        let (arrived, chain, shown_at) = (
            Arc::clone(&arrived),
            Arc::clone(&chain),
            Arc::clone(&shown_at),
        );
        let first = AtomicBool::new(true);
        let weak_for_sink = Arc::downgrade(inner);
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let now = unix_ns();
            if let Some(inner) = weak_for_sink.upgrade() {
                if let Ok(mut last) = inner.last_frame.lock() {
                    *last = Some(Instant::now());
                }
            }
            if first.swap(false, Ordering::Relaxed) {
                // The harness's clock for "share start -> first frame".
                eprintln!(
                    "[DeskLink] phone-screen first frame at unix {:.3}",
                    now as f64 / 1e9
                );
            }
            let at = last_buffer(&info.data)
                .and_then(|buffer| buffer.pts())
                .and_then(|pts| {
                    let mut arrived = arrived.lock().unwrap_or_else(|e| e.into_inner());
                    arrived.remove(&pts.nseconds())
                });
            if let Some((at, rtp)) = at {
                let ms = at.elapsed().as_secs_f64() * 1e3;
                let mut chain = chain.lock().unwrap_or_else(|e| e.into_inner());
                *chain = (chain.0 + ms, chain.1 + 1, chain.2.max(ms));
                if let Some(rtp) = rtp {
                    let mut shown_at = shown_at.lock().unwrap_or_else(|e| e.into_inner());
                    if shown_at.len() < 4096 {
                        shown_at.push((rtp, now));
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    // rtpbin's own session: webrtcbin's `get-stats` had no `inbound-rtp` for
    // this stream, so every line read "0 kbps, ?".
    let read_sources = move || {
        webrtcbin
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("rtpbin"))
            .and_then(|rtpbin| {
                rtpbin.emit_by_name::<Option<gst::glib::Object>>("get-internal-session", &[&0u32])
            })
            .and_then(|session| {
                let stats = session.property::<gst::Structure>("stats");
                let sources = stats.get::<gst::glib::ValueArray>("source-stats").ok()?;
                Some(
                    sources
                        .iter()
                        .filter_map(|value| value.get::<gst::Structure>().ok())
                        .collect::<Vec<_>>(),
                )
            })
    };
    let read_inbound = {
        let read_sources = read_sources.clone();
        move || read_sources().and_then(|sources| inbound_summary(sources.into_iter()))
    };
    let weak = Arc::downgrade(inner);
    std::thread::spawn(move || {
        // The session outlives a share (F28): counted from this share's start.
        // The phone's source reads as a sender only once this share's first
        // packet is in, so the baseline waits for it (F32): read too early it
        // was `None` and the share counted from the connection's start.
        let start = (0..100).find_map(|_| {
            read_inbound().or_else(|| {
                std::thread::sleep(Duration::from_millis(50));
                None
            })
        });
        let mut bytes = start.as_ref().map_or(0, |inbound| inbound.bytes);
        let (mut frames, mut since) = (0u64, Instant::now());
        let mut drops = udp_receive_buffer_drops();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let Some(inner) = weak
                .upgrade()
                .filter(|i| !i.stopped.load(Ordering::Acquire))
            else {
                return;
            };
            let (sum, count, max) =
                std::mem::take(&mut *chain.lock().unwrap_or_else(|e| e.into_inner()));
            let shown = inner.shown.load(Ordering::Relaxed);
            let size = inner
                .sink
                .static_pad("sink")
                .and_then(|pad| pad.current_caps())
                .and_then(|caps| {
                    let s = caps.structure(0)?;
                    Some(format!(
                        "{}x{}",
                        s.get::<i32>("width").ok()?,
                        s.get::<i32>("height").ok()?
                    ))
                })
                .unwrap_or_else(|| "?".into());
            let decoder = inner
                .elements
                .first()
                .and_then(|decode| decode.downcast_ref::<gst::Bin>().cloned())
                .and_then(|bin| {
                    bin.iterate_recurse()
                        .into_iter()
                        .flatten()
                        .find_map(|element| {
                            let factory = element.factory()?;
                            factory
                                .has_type(gst::ElementFactoryType::DECODER)
                                .then(|| factory.name().to_string())
                        })
                })
                .unwrap_or_else(|| "?".into());
            let inbound = read_inbound().map(|now| match &start {
                Some(start) => now.since(start),
                None => now,
            });
            let received = inbound.as_ref().map_or(bytes, |inbound| inbound.bytes);
            let seconds = since.elapsed().as_secs_f64();
            let show = |value: Option<String>| value.unwrap_or_else(|| "?".to_string());
            // Capture on the phone to the sink here, by the phone's sender report.
            let shown_now =
                std::mem::take(&mut *shown_at.lock().unwrap_or_else(|e| e.into_inner()));
            let capture = read_sources()
                .and_then(|sources| sender_report(sources.into_iter()))
                .filter(|_| !shown_now.is_empty())
                .map(|report| {
                    let ms: Vec<f64> = shown_now
                        .iter()
                        .map(|&(rtp, at)| capture_to_shown_ms(report, rtp, at))
                        .collect();
                    // The measurement harness's per-frame record (shown at, capture to sink).
                    if let Some(dir) = frame_dir() {
                        use std::io::Write;
                        if let Ok(mut file) =
                            open_private(dir, "frames.csv", true).and_then(|file| {
                                match file.metadata()?.len() < FRAMES_CSV_MAX_BYTES {
                                    true => Ok(file),
                                    false => Err(std::io::ErrorKind::FileTooLarge.into()),
                                }
                            })
                        {
                            for (&(_, at), ms) in shown_now.iter().zip(&ms) {
                                let _ = writeln!(file, "{:.3},{ms:.1}", at as f64 / 1e6);
                            }
                        }
                    }
                    format!(
                        "{:.1} ms avg / {:.1} max",
                        ms.iter().sum::<f64>() / ms.len() as f64,
                        ms.iter().cloned().fold(f64::MIN, f64::max)
                    )
                })
                .unwrap_or_else(|| "-".into());
            let drops_now = udp_receive_buffer_drops();
            let dropped = drops
                .zip(drops_now)
                .map(|(was, now)| now.saturating_sub(was));
            drops = drops_now;
            eprintln!(
                "[DeskLink] phone-screen-stats: {size} via {decoder}, shown {:.1} fps, in {} kbps, \
                 lost {}, nack {}, pli {}, udp drops {}, jitter {} ms, \
                 after jitter buffer {} ms avg / {:.1} max, capture to sink {capture} (phone clock)",
                (shown - frames) as f64 / seconds,
                ((received.saturating_sub(bytes)) as f64 * 8.0 / seconds / 1000.0) as u64,
                show(inbound.as_ref().map(|i| i.lost.to_string())),
                show(inbound.as_ref().map(|i| i.nacks.to_string())),
                show(inbound.as_ref().map(|i| i.plis.to_string())),
                show(dropped.map(|n| n.to_string())),
                show(inbound.as_ref().and_then(|i| i.jitter_ms).map(|ms| format!("{ms:.1}"))),
                if count == 0 { "-".to_string() } else { format!("{:.1}", sum / f64::from(count)) },
                max,
            );
            (frames, bytes, since) = (shown, received, Instant::now());
        }
    });
}
