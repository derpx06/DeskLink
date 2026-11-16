//! Video SDP and transceiver rules: no video SSRCs in the desk's SDP, the H.264
//! receive level, configuring the video transceivers, which GStreamer versions
//! answer which way, and share ownership of the section.
//!
//! Sibling edges: none. Outside: `screen`.

use super::*;

/// The description as the phone should see it: the video section without
/// `a=ssrc` / `a=ssrc-group` lines.
///
/// `webrtcbin` writes them before any capture is linked, declaring the screen
/// stream's SSRC as **0** (`a=ssrc-group:FID 0 <rtx>`). libwebrtc then binds
/// the receiving track to SSRC 0, and the real stream — arriving later under
/// its true SSRC — is decoded into a default sink no track is connected to:
/// frames decoded, `onTrack` never fired, a phone showing only "waiting for
/// the first picture". Without SSRCs the stream is matched by its section and
/// payload types (libwebrtc's unsignaled path) and reaches the track.
pub(super) fn without_video_ssrcs(sdp: &str) -> String {
    let mut in_video = false;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
        }
        if in_video && (line.starts_with("a=ssrc:") || line.starts_with("a=ssrc-group:")) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The H.264 level this desk decodes, written into its description
/// (contract V2): constrained baseline level 4.0 (`42e028`), so the phone may
/// send up to 1920 px. `webrtcbin` echoes the offer's level (3.1 from
/// libwebrtc, 1280 px), and constraining it through codec preferences would
/// fail the caps intersection, so the text is changed instead; H.264's
/// `level-asymmetry-allowed` lets each side state its own receive level.
pub(crate) const H264_RECEIVE_LEVEL: &str = "42e028";

pub(super) fn with_h264_receive_level(sdp: &str) -> String {
    let mut in_video = false;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
        }
        if in_video && line.starts_with("a=fmtp:") {
            if let Some(at) = line.find("profile-level-id=42e0") {
                let start = at + "profile-level-id=".len();
                if line.len() >= start + 6 {
                    out.push_str(&line[..start]);
                    out.push_str(H264_RECEIVE_LEVEL);
                    out.push_str(&line[start + 6..]);
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    out
}

/// Prepares every video transceiver for what this desktop does with it: send
/// its own screen, and ask for retransmission of lost packets.
///
/// **Direction.** A transceiver `webrtcbin` creates while applying a remote
/// offer starts out receive-only, so the answer to the phone's `sendrecv` offer
/// said `a=recvonly` and the desktop could *never* send a frame — in exactly the
/// pairings where the phone is the deterministic initiator (its device id sorts
/// lower), which is the paired pair this was found on. The initiator's own
/// transceiver is added `sendrecv` already; the answerer's must be raised to
/// match **before** the answer is created, which is why this runs on every
/// remote description right ahead of `create-answer`.
///
/// **NACK.**
/// [`DesktopWebRtcPeer::add_screen_transceiver`] sets `do-nack` on the section
/// the **offerer** creates. The answerer's transceiver is created by
/// `webrtcbin` when it applies the offer, and nothing ever set it there — so in
/// the half of all pairings where the phone is the deterministic initiator, the
/// desktop viewer sat waiting for a keyframe it never requested. The same blank
/// window, from the opposite role.
///
/// Idempotent and cheap, so it runs on every remote description rather than
/// trying to work out which side created what. The index is bounded because
/// `get-transceiver` returning `None` is the only termination signal and a
/// runaway loop here would hang a GStreamer promise callback.
pub(super) fn configure_video_transceivers(webrtcbin: &gst::Element) {
    // Answering: accept only the codecs this desktop wants, with TWCC under
    // the id the offer chose. (The offerer's transceiver carries its caps.)
    let answer_caps = webrtcbin
        .property::<Option<gst_webrtc::WebRTCSessionDescription>>("remote-description")
        .filter(|remote| remote.type_() == gst_webrtc::WebRTCSDPType::Offer)
        .and_then(|remote| remote.sdp().as_text().ok())
        .map(|sdp| {
            crate::device_links::webrtc::screen::answer_video_caps(
                crate::device_links::webrtc::screen::twcc_extmap_id(&sdp),
                answer_with_h264(),
            )
        });
    for index in 0i32..MAX_TRANSCEIVERS {
        let Some(transceiver) = webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&index])
        else {
            return;
        };
        // The audio section (A1) is answered sendrecv too; nothing else.
        if transceiver.kind() == gst_webrtc::WebRTCKind::Audio {
            if transceiver.find_property("direction").is_some() {
                transceiver.set_property(
                    "direction",
                    gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                );
            }
            continue;
        }
        if transceiver.kind() != gst_webrtc::WebRTCKind::Video {
            continue;
        }
        // Guarded: `set_property` panics on an absent property, and this one is
        // reached on transceivers this code did not create.
        if transceiver.find_property("direction").is_some() {
            transceiver.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
            );
        }
        if transceiver.find_property("do-nack").is_some() {
            transceiver.set_property("do-nack", true);
        }
        if let Some(caps) = &answer_caps {
            if transceiver.find_property("codec-preferences").is_some() {
                transceiver.set_property("codec-preferences", caps);
            }
        }
    }
}

/// Whether the answerer must add its own media sections before applying an
/// offer. `webrtcbin` before GStreamer 1.26 creates the answerer's
/// transceivers only while writing the answer, too late to make them
/// `sendrecv`: it answered `recvonly`, so this computer could neither share
/// its screen nor send sound (Ubuntu 24.04, GStreamer 1.24). From 1.26 the
/// offer creates them and an added one would be a second, unmatched section.
pub(super) fn answerer_adds_own_sections(version: (u32, u32, u32, u32)) -> bool {
    (version.0, version.1) < (1, 26)
}

/// Whether an answer offers H.264 next to VP8. `webrtcbin` before 1.28
/// answers only the first of several preferred codecs
/// (`gst_sdp_media_set_media_from_caps`), so H.264 first left out the VP8 this
/// computer sends with and its screen never reached the phone. There the
/// answer is VP8 alone and the phone sends VP8 too (no hardware H.264 from the
/// phone on GStreamer 1.24 and 1.26).
pub(super) fn answer_with_h264() -> bool {
    crate::device_links::webrtc::screen::h264_decoder_available()
        && answers_every_codec(gst::version())
}

pub(super) fn answers_every_codec(version: (u32, u32, u32, u32)) -> bool {
    (version.0, version.1) >= (1, 28)
}

#[cfg(test)]
thread_local! {
    /// Tests: build an offerer without the audio section (an older phone).
    pub(super) static NO_AUDIO_SECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a capture slot held by `current` is another share's, which a
/// retry (`own_only`) must leave alone. A first start replaces anything.
pub(super) fn held_by_another_share(current: Option<u64>, owner: u64, own_only: bool) -> bool {
    own_only && current.is_some_and(|current| current != owner)
}

/// A retried capture found a newer share's capture in its place.
pub(super) const REPLACED_BY_ANOTHER_SHARE: &str = "The desktop share was replaced";

/// A max-bundle session has one media section per kind; this is slack, not a
/// budget.
///
/// `i32` because `get-transceiver` takes a **`gint`**. Passing `guint` panics
/// inside the GStreamer callback with "Incompatible argument type in argument 0"
/// and aborts the process — the same shape as the `ice-agent` property mistake,
/// and equally invisible until the signal actually fires.
const MAX_TRANSCEIVERS: i32 = 16;
