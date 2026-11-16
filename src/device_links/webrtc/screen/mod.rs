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
//!
//! Layout: this file holds the roles, the video caps and TWCC rules, the
//! negotiated section and payload lookups, `set_if_present` and the
//! [`ScreenRequest`] limits. `inbound` decodes the phone's screen,
//! `desktop_capture` captures this desktop for the phone with its `encoder`
//! and `bitrate` controller. No cycles between the parts.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use gst::prelude::*;

mod bitrate;
mod desktop_capture;
mod encoder;
mod inbound;
use bitrate::*;
use encoder::*;
#[cfg(test)]
use inbound::*;

// The crate-facing surface (everything else stays inside).
#[cfg(debug_assertions)]
pub(crate) use desktop_capture::desktop_share_selftest;
pub(crate) use desktop_capture::{DesktopScreenCapture, DesktopSource, RtpContinuity, RtpResume};
pub(crate) use encoder::pipewire_source;
#[cfg(test)]
pub(crate) use inbound::Inbound;
pub(crate) use inbound::{
    attach_incoming_video, clear_stream, forget_parked, park_pad, publish_stream, resume_parked,
    stream_for, while_blocked, ScreenStream,
};

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

/// The pad for video section `mline`. When `webrtcbin` already has it (a
/// chain that never let go, or a pad made for the offer that is not marked
/// as video), it is taken over: asking for it by name a second time is
/// undefined in GStreamer, and failed every later share on the connection.
/// Never a pad of the audio section.
pub(crate) fn section_pad(webrtcbin: &gst::Element, mline: u32) -> Result<gst::Pad, String> {
    let name = format!("sink_{mline}");
    let Some(pad) = webrtcbin.static_pad(&name) else {
        return webrtcbin.request_pad_simple(&name).ok_or_else(|| {
            "Could not attach video to the negotiated WebRTC screen track".to_string()
        });
    };
    let kind = pad
        .find_property("transceiver")
        .and_then(|_| pad.property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver"))
        .map(|transceiver| transceiver.kind());
    let holder = pad
        .peer()
        .and_then(|peer| peer.parent_element())
        .map(|element| element.name().to_string());
    debug_log!(
        "[DeskLink] Screen: {name} already exists (transceiver {kind:?}, linked to {holder:?})"
    );
    if kind == Some(gst_webrtc::WebRTCKind::Audio) {
        return Err(format!(
            "The negotiated video section's pad {name} belongs to the audio track"
        ));
    }
    if let Some(peer) = pad.peer() {
        let _ = peer.unlink(&pad);
    }
    Ok(pad)
}

/// The m-line index of the video section the remote offer negotiated.
pub(crate) fn negotiated_video_mline(webrtcbin: &gst::Element) -> Option<u32> {
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
    negotiated_payload_in(webrtcbin, "video", wanted)
}

/// The payload type of `wanted` in the local description's `kind` section.
pub(crate) fn negotiated_payload_in(
    webrtcbin: &gst::Element,
    kind: &str,
    wanted: &str,
) -> Option<u32> {
    let description =
        webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")?;
    let sdp = description.sdp();
    for media in sdp.medias() {
        if media.media() != Some(kind) {
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
    /// A second display (F2): the phone's view in pixels. `None` mirrors.
    pub extend: Option<(i32, i32)>,
}

impl ScreenRequest {
    /// What the desk serves when the phone asked nothing (older phones):
    /// 1280 px, 30 fps.
    pub(crate) const FALLBACK: Self = Self {
        max_dimension: 1280,
        fps: 30,
        extend: None,
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
            extend: None,
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
            extend: self.extend,
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

/// Applies a property given as a string, only when the element has it. The
/// string form avoids guessing an enum's or flags' Rust type; the guard is
/// because `set_property` panics on an absent name and GStreamer plugins differ
/// between versions.
fn set_if_present(element: &gst::Element, name: &str, value: &str) {
    if element.has_property(name, None) {
        element.set_property_from_str(name, value);
    }
}

#[cfg(test)]
mod tests;
