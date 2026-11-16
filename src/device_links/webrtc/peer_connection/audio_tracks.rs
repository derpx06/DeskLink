//! The peer's audio section (`impl DesktopWebRtcPeer`): adding it, playing
//! what arrives, sending a source, and RTP continuity across sound sessions.
//!
//! Sibling edges: none. Outside: `audio`.

use super::*;

impl DesktopWebRtcPeer {
    /// Adds the one Opus audio section (A1), `sendrecv` and present from the
    /// first offer, after the video section: sound in either direction later
    /// needs no renegotiation (there is none).
    pub(super) fn add_audio_transceiver(&self) -> Result<(), String> {
        self.webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                "add-transceiver",
                &[
                    &gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                    &crate::device_links::webrtc::audio::audio_caps(),
                ],
            )
            .map(|_| ())
            .ok_or_else(|| "Could not add the DeskLink sound section".to_string())
    }

    /// Whether this connection has a live audio section (an older phone's
    /// answer has none).
    pub fn audio_negotiated(&self) -> bool {
        crate::device_links::webrtc::audio::negotiated(&self.webrtcbin)
    }

    /// Plays (or publishes as "DeskLink Microphone") the phone's audio,
    /// replacing whatever played before.
    pub(crate) fn start_audio_playback(
        &self,
        output: crate::device_links::webrtc::audio::Output,
        phone: &str,
    ) -> Result<(), String> {
        self.stop_audio_playback();
        if let Ok(mut wanted) = self.audio_wanted.lock() {
            *wanted = Some((output, phone.to_string()));
        }
        // Not arrived yet: it starts on the stream's first packet.
        let Some(pad) = self.audio_pad.lock().ok().and_then(|pad| pad.clone()) else {
            return Ok(());
        };
        let playback = crate::device_links::webrtc::audio::AudioPlayback::start(
            &self.pipeline,
            &pad,
            output,
            phone,
        )?;
        if let Ok(mut current) = self.audio_playback.lock() {
            *current = Some(playback);
        }
        Ok(())
    }

    /// Whether the phone's audio plays here now, and where.
    pub(crate) fn audio_playback(&self) -> Option<crate::device_links::webrtc::audio::Output> {
        self.audio_playback
            .lock()
            .ok()
            .and_then(|playback| playback.as_ref().map(|playback| playback.output))
    }

    pub(crate) fn audio_received(&self) -> u64 {
        self.audio_playback
            .lock()
            .ok()
            .and_then(|playback| playback.as_ref().map(|playback| playback.received()))
            .unwrap_or(0)
    }

    pub(crate) fn stop_audio_playback(&self) {
        if let Ok(mut wanted) = self.audio_wanted.lock() {
            *wanted = None;
        }
        let playback = self.audio_playback.lock().ok().and_then(|mut p| p.take());
        if let Some(playback) = playback {
            playback.stop();
        }
    }

    /// Sends this computer's sound or microphone to the phone, replacing
    /// whatever was sent before.
    pub(crate) fn start_audio_send(
        &self,
        source: crate::device_links::webrtc::audio::Source,
    ) -> Result<(), String> {
        self.stop_audio_send();
        let send = crate::device_links::webrtc::audio::AudioSend::start(
            &self.pipeline,
            &self.webrtcbin,
            source,
            self.continuity(),
        )?;
        if let Ok(mut current) = self.audio_send.lock() {
            *current = Some(send);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_audio_send_from(
        &self,
        source: crate::device_links::webrtc::audio::Source,
        input: gst::Element,
    ) -> Result<(), String> {
        self.stop_audio_send();
        let send = crate::device_links::webrtc::audio::AudioSend::start_from(
            &self.pipeline,
            &self.webrtcbin,
            source,
            input,
            self.continuity(),
        )?;
        if let Ok(mut current) = self.audio_send.lock() {
            *current = Some(send);
        }
        Ok(())
    }

    /// What this computer sends to the phone now, if anything.
    pub(crate) fn audio_sending(&self) -> Option<crate::device_links::webrtc::audio::Source> {
        self.audio_send
            .lock()
            .ok()
            .and_then(|send| send.as_ref().map(|send| send.source))
    }

    pub(crate) fn audio_sent(&self) -> u64 {
        self.audio_send
            .lock()
            .ok()
            .and_then(|send| send.as_ref().map(|send| send.sent()))
            .unwrap_or(0)
    }

    pub(crate) fn stop_audio_send(&self) {
        let send = self.audio_send.lock().ok().and_then(|mut s| s.take());
        if let Some(send) = send {
            let next = send.stop();
            if let (Some(next), Ok(mut continuity)) = (next, self.audio_continuity.lock()) {
                continuity.next_seqnum = Some(next);
            }
        }
    }

    fn continuity(&self) -> crate::device_links::webrtc::audio::Continuity {
        self.audio_continuity
            .lock()
            .map(|continuity| *continuity)
            .unwrap_or(crate::device_links::webrtc::audio::Continuity {
                ssrc: 1,
                timestamp_offset: 0,
                next_seqnum: None,
            })
    }
}
