//! The desktop share's encoder chain (VP8, or H.264 when asked for), its
//! bitrate setting, the orientation filter, and the PipeWire and X11 sources.
//!
//! Sibling edges: none (the parent's caps and `set_if_present`).

use super::*;

/// Debug builds, `DESKLINK_SCREEN_H264=1`: send the desk's screen as VA-API
/// hardware H.264 instead of software VP8 — an experiment to measure against
/// VP8 (Phase 1c), off unless asked for, and only where VA can encode.
pub(super) fn h264_experiment() -> bool {
    cfg!(debug_assertions)
        && std::env::var("DESKLINK_SCREEN_H264").as_deref() == Ok("1")
        && (gst::ElementFactory::find("vah264lpenc").is_some()
            || gst::ElementFactory::find("vah264enc").is_some())
        && gst::ElementFactory::find("rtph264pay").is_some()
}

/// Sets the encoder's bitrate, whichever unit it takes: VP8's
/// `target-bitrate` in bit/s, VA H.264's `bitrate` in kbit/s.
pub(super) fn set_encoder_bitrate(encoder: &gst::Element, bits_per_second: u32) {
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
pub(super) fn encoder_chain(
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
    // Software, never VA-API `vapostproc`: fed system memory (all PipeWire
    // gives here), its upload and download capped the whole chain at 26-36
    // fps against 47-78 in software, measured on this laptop's 2560x1600
    // panel scaled to 1920x1200 with the same VP8 settings.
    let scale = make("videoscale", "videoscale")?;
    set_if_present(&scale, "method", "lanczos");
    set_if_present(&scale, "n-threads", "4");
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

/// Turns the picture the way the capture's `image-orientation` tag says, and
/// is a pass-through when there is no tag. `None` if `videoflip` is missing
/// (the picture is then sent as captured, as before).
pub(super) fn orientation_filter() -> Option<gst::Element> {
    gst::ElementFactory::make("videoflip")
        .property_from_str("video-direction", "auto")
        .build()
        .ok()
}

/// The PipeWire frame source for a portal-approved stream.
pub(crate) fn pipewire_source(
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
pub(super) fn x11_source() -> Result<gst::Element, String> {
    gst::ElementFactory::make("ximagesrc")
        .property("use-damage", false)
        .property("show-pointer", true)
        .build()
        .map_err(|error| {
            format!("DeskLink screen capture on X11 needs GStreamer ximagesrc (gst-plugins-good): {error}")
        })
}

/// Encoder bitrate start point for the desktop screen (bounds: `bitrate`).
pub(super) const START_BITRATE: u32 = 2_500_000;
