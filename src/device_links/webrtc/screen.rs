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

use std::sync::Mutex;

use gst::prelude::*;

/// Body value of `role` for "send me the phone's screen".
pub const ROLE_PHONE_SCREEN: &str = "phone-screen";
/// Body value of `role` for "send me the desktop's screen".
pub const ROLE_DESKTOP_SCREEN: &str = "desktop-screen";

/// Caps for the negotiated video section.
///
/// VP8 matches what the Android application encodes with, and it is the codec
/// both `webrtcbin` and libwebrtc always have available. Pinning it here keeps
/// the two ends from negotiating something only one of them can decode.
pub(crate) fn video_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "VP8")
        .field("payload", 96i32)
        .field("clock-rate", 90_000i32)
        .build()
}

/// The rendering tail of an inbound screen stream.
///
/// The sink element is held rather than the paintable itself: a
/// `gdk::Paintable` is not `Send`, so it cannot travel from the GStreamer
/// thread that builds this pipeline to the GTK thread that displays it. A
/// `gst::Element` can, and the paintable is read from it on the main thread.
#[derive(Clone)]
pub struct ScreenStream {
    sink: gst::Element,
}

impl ScreenStream {
    /// The GTK paintable this stream renders into.
    ///
    /// Must be called on the main thread — the returned object belongs to GTK.
    pub fn paintable(&self) -> Option<gtk::gdk::Paintable> {
        self.sink
            .property::<Option<gtk::gdk::Paintable>>("paintable")
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
    if let Ok(mut streams) = streams().lock() {
        streams.insert(device_id.to_string(), stream);
    }
}

/// Drops a device's stream when its session or screen share ends.
pub(crate) fn clear_stream(device_id: &str) {
    if let Ok(mut streams) = streams().lock() {
        streams.remove(device_id);
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
    let sink = gst::ElementFactory::make("gtk4paintablesink")
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

    Ok(ScreenStream { sink })
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
    fn negotiated_video_caps_pin_vp8() {
        // Building caps touches the GStreamer type system, so this test needs
        // the library initialised even though it starts no pipeline.
        gst::init().unwrap();

        let caps = video_caps();
        let structure = caps.structure(0).unwrap();
        assert_eq!(structure.get::<&str>("encoding-name").unwrap(), "VP8");
        assert_eq!(structure.get::<&str>("media").unwrap(), "video");
    }
}
