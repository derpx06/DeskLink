//! The peer's screen track (`impl DesktopWebRtcPeer`): adding the video
//! section, starting, retrying and stopping the desktop capture for an owner,
//! RTP continuity across shares, and the answerer's own sections.
//!
//! Sibling edges: `video_sdp`, `audio_tracks` (the answerer adds both sections).
//! Outside: `screen`.

use super::*;

impl DesktopWebRtcPeer {
    /// Starts the portal-approved desktop screen track on the video m-line
    /// negotiated during the initial offer/answer exchange.
    /// Starts the capture for `owner`, replacing any earlier one.
    ///
    /// A share that is starting while the previous one is still winding down
    /// (a retry, or a release followed at once by a new request) must not fail
    /// with "already active": the earlier capture is stopped first.
    pub(crate) fn start_desktop_screen_capture(
        &self,
        owner: u64,
        source: crate::device_links::webrtc::screen::DesktopSource,
        width: i32,
        height: i32,
        request: crate::device_links::webrtc::screen::ScreenRequest,
    ) -> Result<(), String> {
        self.start_capture(owner, false, source, width, height, request)
    }

    /// [`Self::start_desktop_screen_capture`] for a retry by `owner`: it
    /// never takes, or overwrites, a capture another (newer) share started
    /// meanwhile; that share keeps its picture and the retry gives up.
    pub(crate) fn retry_desktop_screen_capture(
        &self,
        owner: u64,
        source: crate::device_links::webrtc::screen::DesktopSource,
        width: i32,
        height: i32,
        request: crate::device_links::webrtc::screen::ScreenRequest,
    ) -> Result<(), String> {
        self.start_capture(owner, true, source, width, height, request)
    }

    fn start_capture(
        &self,
        owner: u64,
        own_only: bool,
        source: crate::device_links::webrtc::screen::DesktopSource,
        width: i32,
        height: i32,
        request: crate::device_links::webrtc::screen::ScreenRequest,
    ) -> Result<(), String> {
        let held_by_another = |slot: &Option<(
            u64,
            crate::device_links::webrtc::screen::DesktopScreenCapture,
        )>| {
            held_by_another_share(slot.as_ref().map(|(current, _)| *current), owner, own_only)
        };
        let previous = {
            let mut slot = self
                .desktop_screen_capture
                .lock()
                .map_err(|_| "DeskLink desktop screen track lock poisoned".to_string())?;
            if held_by_another(&slot) {
                return Err(REPLACED_BY_ANOTHER_SHARE.to_string());
            }
            slot.take()
        };
        if let Some((_, previous)) = previous {
            self.stop_or_retire(previous);
        }
        let created = crate::device_links::webrtc::screen::DesktopScreenCapture::attach(
            &self.pipeline,
            &self.webrtcbin,
            source,
            width,
            height,
            request,
            Some(self.screen_continuity()),
        )?;
        let mut slot = self
            .desktop_screen_capture
            .lock()
            .map_err(|_| "DeskLink desktop screen track lock poisoned".to_string())?;
        if held_by_another(&slot) {
            drop(slot);
            self.stop_or_retire(created);
            return Err(REPLACED_BY_ANOTHER_SHARE.to_string());
        }
        *slot = Some((owner, created));
        Ok(())
    }

    /// Stops whatever capture is active.
    pub fn stop_desktop_screen_capture(&self) {
        let capture = self
            .desktop_screen_capture
            .lock()
            .ok()
            .and_then(|mut capture| capture.take());
        if let Some((_, capture)) = capture {
            self.stop_or_retire(capture);
        }
    }

    /// Stops the capture only if `owner` started it. A worker that finishes
    /// after a newer share has begun must leave that share alone.
    pub fn stop_desktop_screen_capture_for(&self, owner: u64) {
        let capture = self
            .desktop_screen_capture
            .lock()
            .ok()
            .and_then(|mut capture| {
                if capture
                    .as_ref()
                    .is_some_and(|(current, _)| *current == owner)
                {
                    capture.take()
                } else {
                    None
                }
            });
        if let Some((_, capture)) = capture {
            self.stop_or_retire(capture);
        }
    }

    /// The active capture's source size in real pixels, once negotiated.
    pub fn desktop_screen_source_size(&self) -> Option<(i32, i32)> {
        self.desktop_screen_capture.lock().ok().and_then(|capture| {
            capture
                .as_ref()
                .and_then(|(_, capture)| capture.source_size())
        })
    }

    /// RTP packets the active capture has produced, zero when there is none.
    pub fn desktop_screen_frames_sent(&self) -> u64 {
        self.desktop_screen_capture
            .lock()
            .ok()
            .and_then(|capture| capture.as_ref().map(|(_, capture)| capture.frames_sent()))
            .unwrap_or(0)
    }

    /// How the next share carries on this peer's screen stream.
    pub(super) fn screen_continuity(&self) -> crate::device_links::webrtc::screen::RtpContinuity {
        crate::device_links::webrtc::screen::RtpContinuity {
            ssrc: self.screen_ssrc,
            timestamp_offset: self.screen_timestamp_offset,
            resume: self.screen_resume.lock().ok().and_then(|resume| *resume),
        }
    }

    pub(super) fn stop_or_retire(
        &self,
        capture: crate::device_links::webrtc::screen::DesktopScreenCapture,
    ) {
        capture.detach();
        if let (Some(resume), Ok(mut slot)) = (capture.resume(), self.screen_resume.lock()) {
            *slot = Some(resume);
        }
        if let Err(parked) = capture.stop() {
            debug_log!(
                "[DeskLink] Screen capture is parked behind a connection that is not ready; \
                 it will be released when the peer closes"
            );
            if let Ok(mut retired) = self.retired_captures.lock() {
                retired.push(parked);
            }
        }
    }
}

impl DesktopWebRtcPeer {
    /// Adds the bidirectional video section used for screen sharing.
    ///
    /// Only the offerer does this. The answerer's transceiver is created by
    /// `webrtcbin` when it applies an offer that carries video, so adding one
    /// here as well would produce a second, unmatched m-line.
    ///
    /// The section is `sendrecv` and present from the first offer even when
    /// neither side is sharing yet. Adding media later would require
    /// renegotiation, and only the deterministic initiator may offer — so a
    /// responder that wanted to start sharing would have no way to ask.
    pub(super) fn add_screen_transceiver(&self) -> Result<(), String> {
        let caps = crate::device_links::webrtc::screen::video_caps(
            crate::device_links::webrtc::screen::h264_decoder_available(),
        );
        let transceiver = self
            .webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                "add-transceiver",
                &[&gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv, &caps],
            )
            .ok_or_else(|| "Could not add the DeskLink screen media section".to_string())?;
        // Without this the sender stalls waiting for a keyframe it never asks
        // for, and a viewer joining mid-stream sees nothing until the encoder
        // happens to emit one.
        transceiver.set_property("do-nack", true);
        Ok(())
    }

    /// GStreamer before 1.26 only (see [`answerer_adds_own_sections`]): the
    /// answerer's own `sendrecv` video and sound sections, added before the
    /// first offer is applied so `webrtcbin` pairs them with the offer's by
    /// kind. Later offers on the same connection find them already there.
    pub(super) fn add_answer_sections(&self, offer: &str) -> Result<(), String> {
        let has = |kind: &str| {
            offer
                .lines()
                .any(|line| line.starts_with(&format!("m={kind} ")))
        };
        let existing = self
            .webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>("get-transceiver", &[&0i32]);
        if existing.is_some() {
            return Ok(());
        }
        if has("video") {
            let caps = crate::device_links::webrtc::screen::answer_video_caps(
                crate::device_links::webrtc::screen::twcc_extmap_id(offer),
                answer_with_h264(),
            );
            let transceiver = self
                .webrtcbin
                .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                    "add-transceiver",
                    &[&gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv, &caps],
                )
                .ok_or_else(|| "Could not add the DeskLink screen media section".to_string())?;
            transceiver.set_property("do-nack", true);
        }
        if has("audio") {
            self.add_audio_transceiver()?;
        }
        Ok(())
    }
}
