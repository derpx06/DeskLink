//! One desktop WebRTC peer over GStreamer's `webrtcbin` ([`DesktopWebRtcPeer`]):
//! the pipeline, offer/answer and ICE, the fixed data channels with their
//! byte-bounded backlogs, file frames, closing, and the GStreamer plugin check.
//!
//! Layout: this file holds the peer type, its events and the core methods;
//! `screen_share`, `audio_tracks` and `selected_pair` hold the peer's screen, sound and
//! selected-pair methods (one type in several files), `callbacks` the
//! `webrtcbin` signal handlers, `video_sdp` the video SDP rules and
//! `rtp_tuning` the RTP/socket settings. May depend on: `webrtc` (channels,
//! limits, `ice`, `screen`, `audio`, `file_protocol`), `lan_scope`, GStreamer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gst::prelude::*;
use gst_webrtc::{
    WebRTCDataChannel, WebRTCDataChannelState, WebRTCSDPType, WebRTCSessionDescription,
};

use super::ice::IceServerConfig;
use super::{
    SignalingMessageType, WebRtcChannel, EVENTS_HIGH_WATER, EVENTS_LOW_WATER,
    EVENTS_MAX_HELD_BYTES, EVENTS_MAX_HELD_MESSAGES, LIVENESS_MAX_BUFFERED_BYTES,
    MAX_ENVELOPE_BYTES, MAX_WIRE_MESSAGE_BYTES,
};

mod audio_tracks;
mod callbacks;
mod rtp_tuning;
mod screen_share;
mod selected_pair;
mod video_sdp;
use callbacks::*;
use rtp_tuning::*;
#[cfg(test)]
use selected_pair::*;
use video_sdp::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use video_sdp::H264_RECEIVE_LEVEL;

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone)]
pub enum PeerEvent {
    LocalDescription {
        message_type: SignalingMessageType,
        sdp: String,
    },
    IceCandidate {
        sdp_m_line_index: u32,
        candidate: String,
    },
    EndOfCandidates,
    ChannelOpened(WebRtcChannel),
    Envelope {
        channel: WebRtcChannel,
        bytes: Vec<u8>,
        /// Counts against the channel's backlog until handled (dropped).
        credit: Arc<Credit>,
    },
    Binary {
        channel: WebRtcChannel,
        bytes: Vec<u8>,
        credit: Arc<Credit>,
    },
    ConnectionChanged(String),
    /// An inbound screen stream is decoding and ready to be displayed.
    ScreenStream(super::screen::ScreenStream),
    Error(String),
    Closed,
}

/// Received bytes not yet handled, per data channel: a peer that floods,
/// or a worker that stalls, cannot grow memory past this. Over it, a
/// message is dropped (counted, logged).
///
/// File data has no bound here: a frame dropped from the middle of a stream
/// would fail the transfer. The transfer manager bounds it instead
/// (`FILE_QUEUE_MAX_BYTES`) and pauses or restarts the transfer.
const MAX_BACKLOG_FILE_BYTES: usize = usize::MAX / 2;
const MAX_BACKLOG_BYTES: usize = 4 * 1024 * 1024;

/// One received message's share of its channel's backlog, given back when
/// the event is dropped (handled).
#[derive(Debug)]
pub struct Credit {
    backlog: Arc<std::sync::atomic::AtomicUsize>,
    bytes: usize,
}

impl Drop for Credit {
    fn drop(&mut self) {
        self.backlog.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// A share of `backlog` for `bytes`, or `None` when it would pass `max`.
fn take_credit(
    backlog: &Arc<std::sync::atomic::AtomicUsize>,
    bytes: usize,
    max: usize,
) -> Option<Arc<Credit>> {
    let before = backlog.fetch_add(bytes, Ordering::AcqRel);
    if before.saturating_add(bytes) > max {
        backlog.fetch_sub(bytes, Ordering::AcqRel);
        crate::device_links::limits::log_limited(
            "peer-backlog",
            "A peer sent faster than DeskLink could handle; a message was dropped",
        );
        return None;
    }
    Some(Arc::new(Credit {
        backlog: Arc::clone(backlog),
        bytes,
    }))
}

/// A single GStreamer `webrtcbin` peer. It owns no DeskLink feature state: it
/// merely exposes authenticated data-channel and negotiation events to the
/// session coordinator, which remains the sole owner of session validity.
pub struct DesktopWebRtcPeer {
    /// The SSRC every desktop-screen share on this peer sends with. The
    /// stream is not signalled by SSRC (see [`without_video_ssrcs`]), so the
    /// phone binds its picture to the first one it sees; a second share under
    /// a fresh random SSRC never reached the screen — "waiting for the first
    /// picture" whenever Remote input was reopened.
    screen_ssrc: u32,
    /// The screen stream's RTP clock offset, one per peer like its SSRC.
    screen_timestamp_offset: u32,
    /// Where the next share's sequence numbers and picture ids continue from.
    screen_resume: Mutex<Option<super::screen::RtpResume>>,
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    initiator: bool,
    offer_requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    /// The active capture and the worker that owns it: a worker that is only now
    /// finishing must never stop the capture a newer share has started.
    desktop_screen_capture: Mutex<Option<(u64, super::screen::DesktopScreenCapture)>>,
    /// Captures that could not be wound down while the connection was not ready
    /// (see `DesktopScreenCapture::stop`); `close` releases them.
    retired_captures: Mutex<Vec<super::screen::DesktopScreenCapture>>,
    /// The incoming audio pad (parked on a fakesink while no one listens),
    /// once the phone's first audio arrived.
    audio_pad: Arc<Mutex<Option<gst::Pad>>>,
    /// The phone's sound or microphone being played here (F1). Shared
    /// with the pad callback, which moves it to a new incoming stream.
    audio_playback: Arc<Mutex<Option<super::audio::AudioPlayback>>>,
    /// Where the phone's sound should play once its first packet arrives:
    /// its `audio-ready` comes before the stream does.
    audio_wanted: Arc<Mutex<Option<(super::audio::Output, String)>>>,
    /// This computer's sound or microphone going to the phone (F1).
    audio_send: Mutex<Option<super::audio::AudioSend>>,
    /// The sound's SSRC and where its sequence continues.
    audio_continuity: Mutex<super::audio::Continuity>,
    /// Sends on `events` waiting for it to drain below the high-water mark,
    /// in order (see [`EVENTS_HIGH_WATER`]).
    held: Arc<Mutex<Held>>,
}

/// What waits for `events` to drain.
#[derive(Debug, Default)]
pub(crate) struct Held {
    messages: std::collections::VecDeque<String>,
    bytes: usize,
    /// Called when the file-data channel's buffer drains below
    /// `IN_FLIGHT_LOW`: a sender waiting for room tries again.
    file_wake: Option<FileWake>,
}

/// A callback, which has no `Debug` of its own.
#[derive(Clone)]
pub(crate) struct FileWake(std::sync::Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for FileWake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("FileWake")
    }
}

impl Held {
    /// Whether a send of `len` bytes must wait: something already waits (order
    /// matters), or `buffered` is over the high-water mark.
    pub(crate) fn must_wait(&self, buffered: u64) -> bool {
        !self.messages.is_empty() || buffered > EVENTS_HIGH_WATER
    }

    /// Queues `text`; false when the queue is full (the caller reports it).
    pub(crate) fn push(&mut self, text: &str) -> bool {
        if self.messages.len() >= EVENTS_MAX_HELD_MESSAGES
            || self.bytes + text.len() > EVENTS_MAX_HELD_BYTES
        {
            return false;
        }
        self.bytes += text.len();
        self.messages.push_back(text.to_string());
        true
    }

    /// The next message to send, if `buffered` is not over the high-water
    /// mark.
    pub(crate) fn next(&mut self, buffered: u64) -> Option<String> {
        if buffered > EVENTS_HIGH_WATER {
            return None;
        }
        let text = self.messages.pop_front()?;
        self.bytes -= text.len();
        Some(text)
    }
}

impl DesktopWebRtcPeer {
    pub fn new(
        initiator: bool,
        events: Sender<PeerEvent>,
        ice: &IceServerConfig,
    ) -> Result<Self, String> {
        ensure_gstreamer()?;
        let pipeline = gst::Pipeline::new();
        log_pipeline_problems(&pipeline);
        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .build()
            .map_err(|error| format!("DeskLink WebRTC is unavailable: {error}"))?;
        webrtcbin.set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);
        // The jitter buffer defaults to 200ms — on its own more than the entire
        // rest of a realistic 60-90ms glass-to-glass budget for a LAN mirror.
        //
        // It must be set **before** the pipeline reaches Playing: the internal
        // jitterbuffers read it at construction, so setting it later appears to
        // work and silently does nothing. Guarded because `set_property` panics
        // on an absent property, and that exact mistake against `ice-agent`
        // once killed the peer's event worker on every negotiation.
        if webrtcbin.has_property(JITTER_BUFFER_PROPERTY, None) {
            webrtcbin.set_property(JITTER_BUFFER_PROPERTY, JITTER_BUFFER_LATENCY_MS);
        } else {
            debug_log!(
                "[DeskLink] webrtcbin exposes no `{JITTER_BUFFER_PROPERTY}`; \
                 using the default jitter buffer"
            );
        }
        enable_immediate_rtcp_feedback(&webrtcbin);
        retransmit_by_media_section(&webrtcbin);
        // Before the pipeline starts: candidate gathering begins as soon as the
        // element goes to Playing, so restricting it afterwards would be too
        // late for the first negotiation.
        ice.apply_to(&webrtcbin);
        pipeline
            .add(&webrtcbin)
            .map_err(|error| format!("Could not build DeskLink WebRTC pipeline: {error}"))?;

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let offer_requested = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let audio_pad = Arc::new(Mutex::new(None));
        let audio_playback = Arc::new(Mutex::new(None));
        let audio_wanted = Arc::new(Mutex::new(None));
        let held: Arc<Mutex<Held>> = Arc::default();
        install_webrtcbin_callbacks(
            &webrtcbin,
            &pipeline,
            Arc::clone(&channels),
            events.clone(),
            Arc::clone(&audio_pad),
            Arc::clone(&audio_playback),
            Arc::clone(&audio_wanted),
            Arc::clone(&held),
        )?;

        if let Err(error) = pipeline.set_state(gst::State::Playing) {
            // Back to Null before it is dropped: a half-started webrtcbin
            // keeps its ICE agent's sockets and threads otherwise.
            let _ = pipeline.set_state(gst::State::Null);
            return Err(format!(
                "Could not start DeskLink WebRTC pipeline: {error:?}"
            ));
        }

        let peer = Self {
            pipeline,
            webrtcbin,
            channels,
            events,
            initiator,
            offer_requested,
            screen_resume: Mutex::new(None),
            screen_timestamp_offset: {
                let mut bytes = [0u8; 4];
                let _ = openssl::rand::rand_bytes(&mut bytes);
                // u32::MAX is the payloader's "random".
                u32::from_ne_bytes(bytes) % u32::MAX
            },
            screen_ssrc: loop {
                // Non-zero: 0 is what the unlinked sender advertised.
                let mut bytes = [0u8; 4];
                let _ = openssl::rand::rand_bytes(&mut bytes);
                let ssrc = u32::from_ne_bytes(bytes);
                if ssrc != 0 {
                    break ssrc;
                }
            },
            closed,
            desktop_screen_capture: Mutex::new(None),
            retired_captures: Mutex::new(Vec::new()),
            audio_pad,
            audio_playback,
            audio_wanted,
            audio_send: Mutex::new(None),
            held,
            audio_continuity: Mutex::new(super::audio::Continuity {
                ssrc: loop {
                    let mut bytes = [0u8; 4];
                    let _ = openssl::rand::rand_bytes(&mut bytes);
                    let ssrc = u32::from_ne_bytes(bytes);
                    if ssrc != 0 {
                        break ssrc;
                    }
                },
                timestamp_offset: {
                    let mut bytes = [0u8; 4];
                    let _ = openssl::rand::rand_bytes(&mut bytes);
                    u32::from_ne_bytes(bytes)
                },
                next_seqnum: None,
            }),
        };
        if initiator {
            peer.create_local_channels()?;
            peer.add_screen_transceiver()?;
            #[cfg(test)]
            let with_audio = !NO_AUDIO_SECTION.with(|flag| flag.get());
            #[cfg(not(test))]
            let with_audio = true;
            if with_audio {
                peer.add_audio_transceiver()?;
            }
        }
        Ok(peer)
    }

    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    pub fn create_offer(&self) -> Result<(), String> {
        if !self.initiator {
            return Err(
                "Only the deterministic DeskLink WebRTC initiator may create an offer".to_string(),
            );
        }
        request_local_description(
            &self.webrtcbin,
            SignalingMessageType::Offer,
            &self.events,
            &self.offer_requested,
            false,
        )
    }

    pub fn set_remote_description(
        &self,
        message_type: SignalingMessageType,
        sdp: &str,
    ) -> Result<(), String> {
        let sdp_type = match message_type {
            SignalingMessageType::Offer => WebRTCSDPType::Offer,
            SignalingMessageType::Answer => WebRTCSDPType::Answer,
            _ => {
                return Err("Only a WebRTC offer or answer can be set as a description".to_string())
            }
        };
        debug_log!(
            "[DeskLink] Remote {message_type:?} sections: {}",
            sdp.lines()
                .filter(|line| line.starts_with("m=")
                    || line.starts_with("a=mid")
                    || ["a=sendrecv", "a=sendonly", "a=recvonly", "a=inactive"]
                        .contains(&line.trim()))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" | ")
        );
        if sdp_type == WebRTCSDPType::Offer && answerer_adds_own_sections(gst::version()) {
            self.add_answer_sections(sdp)?;
        }
        let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
            .map_err(|error| format!("Invalid DeskLink WebRTC SDP: {error}"))?;
        let description = WebRTCSessionDescription::new(sdp_type, sdp);
        let webrtcbin = self.webrtcbin.clone();
        let events = self.events.clone();
        let offer_requested = Arc::clone(&self.offer_requested);
        let response_to_offer = matches!(message_type, SignalingMessageType::Offer);
        let promise = gst::Promise::with_change_func(move |reply| {
            if reply.is_err() {
                let _ = events.send(PeerEvent::Error(
                    "Could not set the remote DeskLink WebRTC description".to_string(),
                ));
                return;
            }
            // The remote description is what creates the answerer's
            // transceivers, so this is the earliest point they exist.
            configure_video_transceivers(&webrtcbin);
            if response_to_offer {
                let _ = request_local_description(
                    &webrtcbin,
                    SignalingMessageType::Answer,
                    &events,
                    &offer_requested,
                    true,
                );
            }
        });
        self.webrtcbin
            .emit_by_name::<()>("set-remote-description", &[&description, &promise]);
        Ok(())
    }

    pub fn add_ice_candidate(&self, sdp_m_line_index: u32, candidate: &str) -> Result<(), String> {
        if candidate.is_empty() || candidate.len() > 16 * 1024 {
            return Err("Invalid DeskLink WebRTC ICE candidate".to_string());
        }
        self.webrtcbin
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_m_line_index, &candidate]);
        Ok(())
    }

    /// Tells the local ICE agent that the peer has finished gathering.
    ///
    /// `webrtcbin` learns this from `add-ice-candidate` with a **NULL**
    /// candidate — which is why it cannot go through [`Self::add_ice_candidate`],
    /// whose empty-string rejection would swallow it.
    ///
    /// Without this the agent has no reason to conclude its connectivity check
    /// list, so a peer whose candidates all fail sits in `checking` rather than
    /// transitioning to `failed`. That matters far more than it sounds: nothing
    /// in this transport times out a connecting peer, so "never reaches failed"
    /// means "never recovers".
    pub fn end_of_candidates(&self, sdp_m_line_index: u32) {
        self.webrtcbin
            .emit_by_name::<()>("add-ice-candidate", &[&sdp_m_line_index, &None::<String>]);
    }

    /// Bytes already queued on `channel` and not yet handed to the network.
    ///
    /// The only real backpressure signal available. libwebrtc destroys a data
    /// channel with `RESOURCE_EXHAUSTED` once ~16 MiB is queued, so a producer
    /// that can outrun the link — a terminal running `yes` — must consult this
    /// and stop reading its source rather than queueing without bound.
    ///
    /// An unknown or closed channel reports `u64::MAX` so callers treat it as
    /// fully congested and send nothing.
    pub fn buffered_amount(&self, channel: WebRtcChannel) -> u64 {
        let Ok(channels) = self.channels.lock() else {
            return u64::MAX;
        };
        let Some(channel) = channels.get(&channel) else {
            return u64::MAX;
        };
        if channel.ready_state() != WebRTCDataChannelState::Open {
            return u64::MAX;
        }
        channel.property::<u64>("buffered-amount")
    }

    pub fn send_text(&self, kind: WebRtcChannel, text: &str) -> Result<(), String> {
        // Over 64 KiB would wreck the SCTP association, not just fail: see
        // [`MAX_WIRE_MESSAGE_BYTES`].
        if text.len() > MAX_WIRE_MESSAGE_BYTES.min(MAX_ENVELOPE_BYTES) {
            return Err(format!(
                "DeskLink WebRTC message is too large to send ({} bytes, 64 KiB at most)",
                text.len()
            ));
        }
        let channel = self
            .channels
            .lock()
            .map_err(|_| "DeskLink WebRTC channel lock poisoned".to_string())?
            .get(&kind)
            .cloned()
            .ok_or_else(|| "DeskLink WebRTC data channel is not open".to_string())?;
        if channel.ready_state() != WebRTCDataChannelState::Open {
            return Err("DeskLink WebRTC data channel is not open".to_string());
        }
        if kind == WebRtcChannel::Events {
            let mut held = self
                .held
                .lock()
                .map_err(|_| "DeskLink WebRTC send queue lock poisoned".to_string())?;
            if held.must_wait(channel.property::<u64>("buffered-amount")) {
                return if held.push(text) {
                    Ok(())
                } else {
                    Err(
                        "The connection to the phone is too busy; try again in a moment"
                            .to_string(),
                    )
                };
            }
        }
        channel
            .send_string_full(Some(text))
            .map_err(|error| format!("Could not send DeskLink WebRTC data: {error}"))
    }

    /// Whether one data channel is open.
    pub fn channel_open(&self, kind: WebRtcChannel) -> bool {
        self.channels.lock().is_ok_and(|channels| {
            channels
                .get(&kind)
                .is_some_and(|channel| channel.ready_state() == WebRTCDataChannelState::Open)
        })
    }

    /// Whether media can flow now: the connection is up and the channels a
    /// feature-ready link needs are open. Until then `webrtcbin` holds the
    /// screen capture's thread in its send path.
    pub fn media_ready(&self) -> bool {
        self.webrtcbin
            .property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
            == gst_webrtc::WebRTCPeerConnectionState::Connected
            && self.required_channels_open()
    }

    /// Whether the channels a link needs before it is feature-ready are open.
    pub fn required_channels_open(&self) -> bool {
        let Ok(channels) = self.channels.lock() else {
            return false;
        };
        WebRtcChannel::REQUIRED.iter().all(|kind| {
            channels
                .get(kind)
                .is_some_and(|channel| channel.ready_state() == WebRTCDataChannelState::Open)
        })
    }

    /// Sends a heartbeat on the liveness channel, or skips it (`Ok(false)`)
    /// when more than [`LIVENESS_MAX_BUFFERED_BYTES`] is already queued: the
    /// next one supersedes it.
    pub fn send_heartbeat(&self, text: &str) -> Result<bool, String> {
        if self.buffered_amount(WebRtcChannel::Liveness) > LIVENESS_MAX_BUFFERED_BYTES {
            return Ok(false);
        }
        self.send_text(WebRtcChannel::Liveness, text).map(|()| true)
    }

    /// What to do when the file-data channel's buffer has drained.
    pub fn set_file_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut held) = self.held.lock() {
            held.file_wake = Some(FileWake(std::sync::Arc::new(wake)));
        }
    }

    /// Sends one frame over the dedicated file-data channel. Feature
    /// envelopes always use text channels; accepting arbitrary binary
    /// payloads on them would make channel authorization ambiguous.
    pub fn send_file_frame(&self, frame: Vec<u8>) -> Result<(), String> {
        validate_file_data(&frame)?;
        let channel = self
            .channels
            .lock()
            .map_err(|_| "DeskLink WebRTC channel lock poisoned".to_string())?
            .get(&WebRtcChannel::FileData)
            .cloned()
            .ok_or_else(|| "DeskLink WebRTC file-data channel is not open".to_string())?;
        if channel.ready_state() != WebRTCDataChannelState::Open {
            return Err("DeskLink WebRTC file-data channel is not open".to_string());
        }
        let data = gst::glib::Bytes::from_owned(frame);
        channel
            .send_data_full(Some(&data))
            .map_err(|error| format!("Could not send DeskLink WebRTC file data: {error}"))
    }

    /// Drops every buffer that arrives at any `sctpdec` in this peer from now
    /// on (a pad probe on its sink; it stays until the element is freed).
    ///
    /// GStreamer's `sctpdec` has a `g_assert (src_pad)` in `on_receive`: data
    /// on an SCTP stream whose source pad is gone aborts the process. The
    /// pad goes when a data channel is closed, and the far side (the phone,
    /// or the other peer in a test) can still have data in flight for that
    /// stream for about a round trip. Ceiling: data usrsctp delivers from a
    /// thread of its own, outside the pad's chain function, is not covered,
    /// and neither is an element built after this call (none is, once the
    /// peer is closed).
    fn shut_inbound_data(&self) {
        let Some(bin) = self.pipeline.dynamic_cast_ref::<gst::Bin>() else {
            return;
        };
        for element in bin.iterate_recurse().into_iter().flatten() {
            let is_sctpdec = element
                .factory()
                .is_some_and(|factory| factory.name() == "sctpdec");
            if !is_sctpdec {
                continue;
            }
            if let Some(sink) = element.static_pad("sink") {
                let _ = sink.add_probe(
                    gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                    |_, _| gst::PadProbeReturn::Drop,
                );
                // A buffer that got past the probe before it was added may be
                // anywhere between the pad and `on_receive` (a busy machine
                // makes that long): deactivating the pad waits for the
                // streaming thread to leave the chain function, so nothing is
                // in flight when this returns. The callbacks below it only
                // send on a channel, so the wait is short.
                let _ = sink.set_active(false);
            }
        }
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // Nothing more is wanted from the far side, and nothing it still sends
        // may reach a data channel's `sctpdec` pad once closing the channels
        // takes the pads away: `sctpdec` asserts that a pad exists for every
        // stream it receives on, and the assertion aborts the whole process.
        // So the way in is shut first.
        self.shut_inbound_data();
        // Sound first: its sources are live and push into webrtcbin.
        self.stop_audio_send();
        self.stop_audio_playback();
        if let Ok(channels) = self.channels.lock() {
            for channel in channels.values() {
                channel.close();
            }
        }
        // webrtcbin's `close` action is GStreamer 1.26+ (Ubuntu 24.04 has
        // 1.24, where emitting it panicked); without it the NULL state below
        // closes the connection.
        if gst::glib::subclass::SignalId::lookup("close", self.webrtcbin.type_()).is_some() {
            let promise = gst::Promise::new();
            self.webrtcbin.emit_by_name::<()>("close", &[&promise]);
        }
        // The whole pipeline goes to NULL *before* the screen capture is touched.
        // `webrtcbin` is the downstream end, so it shuts down first, and that is
        // what frees a capture thread parked inside it; stopping the capture
        // first would wait on that thread forever whenever the connection was
        // still coming up or recovering.
        let _ = self.pipeline.set_state(gst::State::Null);
        // And let go of everything that could keep the pipeline alive: nothing
        // may call back into a closed peer, and a replaced peer must be freed.
        if let Some(bus) = self.pipeline.bus() {
            bus.unset_sync_handler();
        }
        let _ = self.pipeline.remove(&self.webrtcbin);
        if let Ok(mut pad) = self.audio_pad.lock() {
            *pad = None;
        }
        if let Ok(mut held) = self.held.lock() {
            held.file_wake = None;
        }
        if let Ok(mut capture) = self.desktop_screen_capture.lock() {
            if let Some((_, capture)) = capture.take() {
                capture.abandon();
            }
        }
        if let Ok(mut retired) = self.retired_captures.lock() {
            for capture in retired.drain(..) {
                capture.abandon();
            }
        }
        let _ = self.events.send(PeerEvent::Closed);
    }

    fn create_local_channels(&self) -> Result<(), String> {
        for channel in WebRtcChannel::ALL {
            let options = channel_options(channel);
            let created = self
                .webrtcbin
                .emit_by_name::<Option<WebRTCDataChannel>>(
                    "create-data-channel",
                    &[&channel.label(), &options],
                )
                .ok_or_else(|| {
                    format!(
                        "Could not create DeskLink WebRTC channel {}",
                        channel.label()
                    )
                })?;
            install_data_channel(
                created,
                Arc::clone(&self.channels),
                self.events.clone(),
                Arc::clone(&self.held),
            )?;
        }
        Ok(())
    }
}

impl Drop for DesktopWebRtcPeer {
    fn drop(&mut self) {
        self.close();
    }
}

/// The GStreamer elements a WebRTC link cannot work without: webrtcbin
/// (gst-plugins-bad), DTLS and libnice's ICE elements.
pub(crate) const REQUIRED_ELEMENTS: [&str; 4] = ["webrtcbin", "dtlsdec", "dtlssrtpenc", "nicesrc"];

/// The names in [`REQUIRED_ELEMENTS`] that `has` does not find.
fn missing_elements(has: impl Fn(&str) -> bool) -> Vec<&'static str> {
    REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|name| !has(name))
        .collect()
}

/// What to tell the person when elements are missing (plain words, what to
/// install), or `None`.
fn missing_words(missing: &[&str]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "DeskLink needs the GStreamer WebRTC plugins (gst-plugins-bad and libnice) installed on this computer. Missing: {}.",
        missing.join(", ")
    ))
}

/// The start-up check: `Some(words)` when this computer lacks what WebRTC
/// needs. Without it every connection failed and the phone read "Can't reach"
/// while the cause was here.
pub(crate) fn plugin_problem() -> Option<String> {
    gst::init().ok()?;
    missing_words(&missing_elements(|name| {
        gst::ElementFactory::find(name).is_some()
    }))
}

fn ensure_gstreamer() -> Result<(), String> {
    GST_INIT
        .get_or_init(|| {
            gst::init().map_err(|error| error.to_string())?;
            register_bundled_paintable_sink();
            Ok(())
        })
        .clone()
}

/// The phone's screen is shown through `gtk4paintablesink`, which some
/// distributions (Ubuntu 24.04) do not package. The distro's own build is
/// preferred when present; otherwise the copy compiled into DeskLink is
/// registered so the viewer works everywhere.
fn register_bundled_paintable_sink() {
    if gst::ElementFactory::find("gtk4paintablesink").is_some() {
        return;
    }
    if let Err(error) = gstgtk4::plugin_register_static() {
        eprintln!("DeskLink could not register its bundled gtk4paintablesink: {error}");
    }
}

fn validate_file_data(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES.min(MAX_WIRE_MESSAGE_BYTES) {
        return Err("DeskLink WebRTC file-data chunk is outside the allowed size".to_string());
    }
    Ok(())
}

fn local_ice_event(sdp_m_line_index: u32, candidate: String) -> PeerEvent {
    if candidate.is_empty() {
        PeerEvent::EndOfCandidates
    } else if candidate.len() > 16 * 1024 {
        PeerEvent::Error("Received an oversized local DeskLink WebRTC ICE candidate".to_string())
    } else {
        PeerEvent::IceCandidate {
            sdp_m_line_index,
            candidate,
        }
    }
}

fn channel_options(channel: WebRtcChannel) -> gst::Structure {
    let mut builder = gst::Structure::builder("application/x-desklink-data-channel")
        .field("ordered", channel.ordered())
        .field("priority", channel.priority());
    if let Some(max_retransmits) = channel.max_retransmits() {
        builder = builder.field("max-retransmits", i32::from(max_retransmits));
    }
    builder.build()
}

/// Whether a newly added pad carries video.
///
/// Data channels and any future audio section appear on the same signal, and
/// building a video decode chain for them would fail in a confusing place.
/// Whether a `webrtcbin` source pad carries the video track.
///
/// The pad's own transceiver is asked first. Caps alone are not enough:
/// `webrtcbin`'s `src_%u` template caps are a bare `application/x-rtp` with no
/// `media` field, so when `current_caps()` is still `None` at `pad-added` time
/// — which is the common case — the old fallback tested
/// `"application/x-rtp".starts_with("video/")`, returned false, and the video
/// stream was silently dropped.
/// Logs errors and warnings any element in `pipeline` posts.
///
/// Nothing else consumes the bus, so a failing source or encoder — a PipeWire
/// stream the compositor tore down, a caps mismatch — used to be completely
/// silent: the share "started", no picture arrived, and there was no line in any
/// log to say why. Messages are dropped after logging so the bus queue cannot
/// grow without bound.
fn log_pipeline_problems(pipeline: &gst::Pipeline) {
    let Some(bus) = pipeline.bus() else {
        return;
    };
    bus.set_sync_handler(|_, message| {
        match message.view() {
            gst::MessageView::Error(error) => {
                let source = error
                    .src()
                    .map(|source| source.path_string().to_string())
                    .unwrap_or_default();
                eprintln!(
                    "[DeskLink] GStreamer error from {source}: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_default()
                );
            }
            gst::MessageView::Warning(warning) => eprintln!(
                "[DeskLink] GStreamer warning from {}: {} ({})",
                warning
                    .src()
                    .map(|source| source.path_string().to_string())
                    .unwrap_or_default(),
                warning.error(),
                warning.debug().unwrap_or_default()
            ),
            _ => {}
        }
        gst::BusSyncReply::Drop
    });
}

#[cfg(test)]
pub(crate) mod tests;
