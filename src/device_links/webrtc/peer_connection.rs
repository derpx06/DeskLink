use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use gst::prelude::*;
use gst_webrtc::{
    WebRTCDataChannel, WebRTCDataChannelState, WebRTCSDPType, WebRTCSessionDescription,
};

use super::ice::IceServerConfig;
use super::{SignalingMessageType, WebRtcChannel, MAX_ENVELOPE_BYTES};

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

const JITTER_BUFFER_PROPERTY: &str = "latency";

/// Milliseconds of jitter buffering, against a 200ms default.
///
/// Both peers are on the same LAN and the session is interactive — a mirror you
/// are clicking on — so smoothing for hundreds of milliseconds of reordering
/// buys nothing and costs everything. 40ms still absorbs ordinary WiFi jitter.
///
/// The sink renders without clock sync (`attach_incoming_video`): measured on
/// a phone mirror, clock sync let the display fall 3–5 s behind as the phone's
/// timestamps drifted, so this buffer is the only smoothing left.
const JITTER_BUFFER_LATENCY_MS: u32 = 40;

/// Floor for the regular RTCP interval of each RTP session.
///
/// GStreamer follows RFC 4585 strictly: one *early* RTCP packet per regular
/// interval, everything else waits for the next regular report — and with the
/// default bandwidth-derived interval that wait was seconds. Measured on a
/// phone mirror: retransmission requests "not sent early, sending with next
/// regular RTCP", 95 of them expiring unanswered, and 3–5 s freezes because
/// the keyframe request queued behind them too. libwebrtc sends NACK/PLI
/// immediately; a 20 ms floor gets within one frame of that.
const RTCP_MIN_INTERVAL_NS: u64 = 20_000_000;
/// RTCP budget in bytes per second, so the bandwidth rule does not stretch the
/// interval back out. It is a ceiling, not traffic: at 8 kB/s compound reports
/// carrying feedback still came out at a 280 ms interval, with early NACKs
/// refused inside it. Real use stays a few percent of the video stream.
const RTCP_BYTES_PER_SECOND: f64 = 64_000.0;

/// Makes every RTP session `webrtcbin` creates send feedback promptly.
/// Sessions only exist after negotiation, so this hooks their first SSRC.
fn enable_immediate_rtcp_feedback(webrtcbin: &gst::Element) {
    let Some(rtpbin) = webrtcbin
        .downcast_ref::<gst::Bin>()
        .and_then(|bin| bin.by_name("rtpbin"))
    else {
        debug_log!("[DeskLink] webrtcbin has no rtpbin; RTCP feedback keeps its default timing");
        return;
    };
    rtpbin.connect("on-new-ssrc", false, |values| {
        let rtpbin = values[0].get::<gst::Element>().ok()?;
        let session_id = values[1].get::<u32>().ok()?;
        let session = rtpbin.emit_by_name::<Option<gst::Element>>("get-session", &[&session_id])?;
        if session.has_property("rtcp-min-interval", None) {
            session.set_property("rtcp-min-interval", RTCP_MIN_INTERVAL_NS);
        }
        if session.has_property("rtcp-fraction", None) {
            session.set_property("rtcp-fraction", RTCP_BYTES_PER_SECOND);
        }
        None
    });
}

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
/// message is dropped (counted, logged): a file chunk then fails its
/// transfer, which resumes.
const MAX_BACKLOG_FILE_BYTES: usize = 32 * 1024 * 1024;
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
    /// Where the next share's sequence numbers continue from.
    screen_next_seqnum: Mutex<Option<u16>>,
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
        install_webrtcbin_callbacks(
            &webrtcbin,
            &pipeline,
            Arc::clone(&channels),
            events.clone(),
            Arc::clone(&audio_pad),
            Arc::clone(&audio_playback),
            Arc::clone(&audio_wanted),
        )?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| format!("Could not start DeskLink WebRTC pipeline: {error:?}"))?;

        let peer = Self {
            pipeline,
            webrtcbin,
            channels,
            events,
            initiator,
            offer_requested,
            screen_next_seqnum: Mutex::new(None),
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
        source: super::screen::DesktopSource,
        width: i32,
        height: i32,
        request: super::screen::ScreenRequest,
    ) -> Result<(), String> {
        let previous = self
            .desktop_screen_capture
            .lock()
            .map_err(|_| "DeskLink desktop screen track lock poisoned".to_string())?
            .take();
        if let Some((_, previous)) = previous {
            self.stop_or_retire(previous);
        }
        let created = super::screen::DesktopScreenCapture::attach(
            &self.pipeline,
            &self.webrtcbin,
            source,
            width,
            height,
            request,
            Some(super::screen::RtpContinuity {
                ssrc: self.screen_ssrc,
                next_seqnum: self.screen_next_seqnum.lock().ok().and_then(|next| *next),
            }),
        )?;
        *self
            .desktop_screen_capture
            .lock()
            .map_err(|_| "DeskLink desktop screen track lock poisoned".to_string())? =
            Some((owner, created));
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

    /// Whether the active capture is scaled by `vapostproc`.
    pub fn desktop_screen_va_scaled(&self) -> bool {
        self.desktop_screen_capture
            .lock()
            .ok()
            .and_then(|capture| capture.as_ref().map(|(_, capture)| capture.va_scaled()))
            .unwrap_or(false)
    }

    fn stop_or_retire(&self, capture: super::screen::DesktopScreenCapture) {
        if let (Some(next), Ok(mut slot)) = (capture.next_seqnum(), self.screen_next_seqnum.lock())
        {
            *slot = Some(next);
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

    /// How the selected ICE pair reaches the peer, per [`path_label`], and
    /// its current round-trip time in milliseconds when known.
    /// `via_relay` is whether the bootstrap link is the relay.
    /// `None` until a pair is selected.
    pub fn path(&self, via_relay: bool) -> Option<(&'static str, Option<u32>)> {
        let (types, rtt) = self.selected_pair_types()?;
        // Until both candidates are resolved the path is unknown, not
        // "direct": guessing showed "over the internet" for a moment on
        // every LAN connect.
        if types.len() != 2 || types.iter().any(String::is_empty) {
            return None;
        }
        Some((path_label(&types, via_relay), rtt))
    }

    /// The selected pair's two candidate types as webrtcbin spells them, and
    /// its round-trip time in milliseconds when known.
    pub fn selected_pair_types(&self) -> Option<(Vec<String>, Option<u32>)> {
        let promise = gst::Promise::new();
        self.webrtcbin
            .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        if promise.wait() != gst::PromiseResult::Replied {
            return None;
        }
        let reply = promise.get_reply()?;
        let stats: Vec<gst::Structure> = reply
            .iter()
            .filter_map(|(_, value)| value.get::<gst::Structure>().ok())
            .collect();
        let field = |stats: &gst::Structure, name: &str| stats.get::<String>(name).ok();
        let by_id = |id: &str| {
            stats
                .iter()
                .find(|entry| field(entry, "id").as_deref() == Some(id))
        };
        let pair_id = stats
            .iter()
            .find_map(|entry| field(entry, "selected-candidate-pair-id"))?;
        let pair = by_id(&pair_id)?;
        let types: Vec<String> = ["local-candidate-id", "remote-candidate-id"]
            .iter()
            .filter_map(|key| field(pair, key))
            .filter_map(|id| by_id(&id).and_then(|candidate| field(candidate, "candidate-type")))
            .collect();
        let rtt = pair
            .get::<f64>("current-round-trip-time")
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| (seconds * 1000.0).round().max(1.0) as u32);
        Some((types, rtt))
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

    pub fn send_text(&self, channel: WebRtcChannel, text: &str) -> Result<(), String> {
        if text.len() > MAX_ENVELOPE_BYTES {
            return Err("DeskLink WebRTC envelope is too large".to_string());
        }
        let channel = self
            .channels
            .lock()
            .map_err(|_| "DeskLink WebRTC channel lock poisoned".to_string())?
            .get(&channel)
            .cloned()
            .ok_or_else(|| "DeskLink WebRTC data channel is not open".to_string())?;
        if channel.ready_state() != WebRTCDataChannelState::Open {
            return Err("DeskLink WebRTC data channel is not open".to_string());
        }
        channel
            .send_string_full(Some(text))
            .map_err(|error| format!("Could not send DeskLink WebRTC data: {error}"))
    }

    /// Sends a bounded binary payload over the dedicated file-data channel.
    /// Feature envelopes always use text channels; accepting arbitrary binary
    /// payloads on them would make channel authorization ambiguous.
    pub fn send_file_data(&self, bytes: &[u8]) -> Result<(), String> {
        validate_file_data(bytes)?;
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
        let data = gst::glib::Bytes::from_owned(bytes.to_vec());
        channel
            .send_data_full(Some(&data))
            .map_err(|error| format!("Could not send DeskLink WebRTC file data: {error}"))
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // Sound first: its sources are live and push into webrtcbin.
        self.stop_audio_send();
        self.stop_audio_playback();
        if let Ok(channels) = self.channels.lock() {
            for channel in channels.values() {
                channel.close();
            }
        }
        let promise = gst::Promise::new();
        self.webrtcbin.emit_by_name::<()>("close", &[&promise]);
        // The whole pipeline goes to NULL *before* the screen capture is touched.
        // `webrtcbin` is the downstream end, so it shuts down first, and that is
        // what frees a capture thread parked inside it; stopping the capture
        // first would wait on that thread forever whenever the connection was
        // still coming up or recovering.
        let _ = self.pipeline.set_state(gst::State::Null);
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

    /// Adds the one Opus audio section (A1), `sendrecv` and present from the
    /// first offer, after the video section: sound in either direction later
    /// needs no renegotiation (there is none).
    fn add_audio_transceiver(&self) -> Result<(), String> {
        self.webrtcbin
            .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                "add-transceiver",
                &[
                    &gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                    &super::audio::audio_caps(),
                ],
            )
            .map(|_| ())
            .ok_or_else(|| "Could not add the DeskLink sound section".to_string())
    }

    /// Whether this connection has a live audio section (an older phone's
    /// answer has none).
    pub fn audio_negotiated(&self) -> bool {
        super::audio::negotiated(&self.webrtcbin)
    }

    /// Plays (or publishes as "DeskLink Microphone") the phone's audio,
    /// replacing whatever played before.
    pub(crate) fn start_audio_playback(
        &self,
        output: super::audio::Output,
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
        let playback = super::audio::AudioPlayback::start(&self.pipeline, &pad, output, phone)?;
        if let Ok(mut current) = self.audio_playback.lock() {
            *current = Some(playback);
        }
        Ok(())
    }

    /// Whether the phone's audio plays here now, and where.
    pub(crate) fn audio_playback(&self) -> Option<super::audio::Output> {
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
    pub(crate) fn start_audio_send(&self, source: super::audio::Source) -> Result<(), String> {
        self.stop_audio_send();
        let send = super::audio::AudioSend::start(
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
        source: super::audio::Source,
        input: gst::Element,
    ) -> Result<(), String> {
        self.stop_audio_send();
        let send = super::audio::AudioSend::start_from(
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
    pub(crate) fn audio_sending(&self) -> Option<super::audio::Source> {
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

    fn continuity(&self) -> super::audio::Continuity {
        self.audio_continuity
            .lock()
            .map(|continuity| *continuity)
            .unwrap_or(super::audio::Continuity {
                ssrc: 1,
                timestamp_offset: 0,
                next_seqnum: None,
            })
    }

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
    fn add_screen_transceiver(&self) -> Result<(), String> {
        let caps = super::screen::video_caps(super::screen::h264_decoder_available());
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
            install_data_channel(created, Arc::clone(&self.channels), self.events.clone())?;
        }
        Ok(())
    }
}

impl Drop for DesktopWebRtcPeer {
    fn drop(&mut self) {
        self.close();
    }
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
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES {
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
        .field("ordered", channel.ordered());
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
                // The share's scaler: the watchdog's rebuild scales in software.
                if source.contains("vapostproc") {
                    super::screen::va_scaler_failed("element error");
                }
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
fn without_video_ssrcs(sdp: &str) -> String {
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

fn with_h264_receive_level(sdp: &str) -> String {
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
fn configure_video_transceivers(webrtcbin: &gst::Element) {
    // Answering: accept only the codecs this desktop wants, with TWCC under
    // the id the offer chose. (The offerer's transceiver carries its caps.)
    let answer_caps = webrtcbin
        .property::<Option<gst_webrtc::WebRTCSessionDescription>>("remote-description")
        .filter(|remote| remote.type_() == gst_webrtc::WebRTCSDPType::Offer)
        .and_then(|remote| remote.sdp().as_text().ok())
        .map(|sdp| {
            super::screen::answer_video_caps(
                super::screen::twcc_extmap_id(&sdp),
                super::screen::h264_decoder_available(),
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

#[cfg(test)]
thread_local! {
    /// Tests: build an offerer without the audio section (an older phone).
    static NO_AUDIO_SECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A max-bundle session has one media section per kind; this is slack, not a
/// budget.
///
/// `i32` because `get-transceiver` takes a **`gint`**. Passing `guint` panics
/// inside the GStreamer callback with "Incompatible argument type in argument 0"
/// and aborts the process — the same shape as the `ice-agent` property mistake,
/// and equally invisible until the signal actually fires.
const MAX_TRANSCEIVERS: i32 = 16;

/// The kind of media an inbound pad carries. Its caps come first: with max
/// bundle, `webrtcbin` hands every receive pad the *first* transceiver (the
/// audio pad claimed to be video, and would have been decoded as a screen).
fn pad_media(pad: &gst::Pad) -> Option<String> {
    let from_caps = pad.current_caps().and_then(|caps| {
        caps.structure(0)
            .and_then(|structure| structure.get::<&str>("media").ok().map(str::to_string))
    });
    if from_caps.is_some() {
        return from_caps;
    }
    if pad.find_property("transceiver").is_some() {
        if let Some(transceiver) =
            pad.property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver")
        {
            match transceiver.kind() {
                gst_webrtc::WebRTCKind::Audio => return Some("audio".into()),
                gst_webrtc::WebRTCKind::Video => return Some("video".into()),
                _ => {}
            }
        }
    }
    pad.query_caps(None).structure(0).and_then(|structure| {
        structure
            .get::<&str>("media")
            .ok()
            .map(str::to_string)
            .or_else(|| {
                let name = structure.name();
                (name.starts_with("video/") || name.starts_with("audio/"))
                    .then(|| name.split('/').next().unwrap_or_default().to_string())
            })
    })
}

fn pad_carries_audio(pad: &gst::Pad) -> bool {
    pad_media(pad).as_deref() == Some("audio")
}

fn pad_carries_video(pad: &gst::Pad) -> bool {
    pad_media(pad).as_deref() == Some("video")
}

fn install_webrtcbin_callbacks(
    webrtcbin: &gst::Element,
    pipeline: &gst::Pipeline,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    audio_pad: Arc<Mutex<Option<gst::Pad>>>,
    audio_playback: Arc<Mutex<Option<super::audio::AudioPlayback>>>,
    audio_wanted: Arc<Mutex<Option<(super::audio::Output, String)>>>,
) -> Result<(), String> {
    // `on-negotiation-needed` deliberately does nothing.
    //
    // It fires as soon as the *first* channel or transceiver is created, and it
    // used to build the offer right there: while the other six channels and the
    // video section were still being added. Whatever the race produced was the
    // offer — in the loopback test, an SDP with **no media sections at all** —
    // and the latch then swallowed the coordinator's own `create_offer()`. The
    // desktop-initiator path (whenever the phone's id sorts higher) therefore
    // could never connect. Everything is negotiated up front, so the offer is
    // requested exactly once, explicitly, after construction has finished.
    webrtcbin.connect("on-negotiation-needed", false, |_| None);

    let ice_events = events.clone();
    webrtcbin.connect("on-ice-candidate", false, move |values| {
        let sdp_m_line_index = values[1].get::<u32>().unwrap_or_default();
        let candidate = values[2].get::<String>().unwrap_or_else(|_| String::new());
        let _ = ice_events.send(local_ice_event(sdp_m_line_index, candidate));
        None
    });

    let incoming_channels = Arc::clone(&channels);
    let incoming_events = events.clone();
    webrtcbin.connect("on-data-channel", false, move |values| {
        match values[1].get::<WebRTCDataChannel>() {
            Ok(channel) => {
                let _ = install_data_channel(
                    channel,
                    Arc::clone(&incoming_channels),
                    incoming_events.clone(),
                );
            }
            Err(_) => {
                let _ = incoming_events.send(PeerEvent::Error(
                    "DeskLink WebRTC supplied an invalid data channel".to_string(),
                ));
            }
        }
        None
    });

    // Inbound media. `webrtcbin` exposes a received stream as a new source pad
    // once it has been negotiated; until then there is nothing to attach to.
    let media_events = events.clone();
    let media_pipeline = pipeline.clone();
    webrtcbin.connect_pad_added(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        // Sound (F1): parked until someone listens, never left unlinked.
        if pad_carries_audio(pad) {
            super::screen::park_pad(&media_pipeline, pad);
            if let Ok(mut slot) = audio_pad.lock() {
                *slot = Some(pad.clone());
            }
            // A new incoming stream (the phone restarted its sound): what was
            // playing moves to it, or what was asked before it arrived starts.
            if let Ok(mut playing) = audio_playback.lock() {
                if let Some(old) = playing.take() {
                    old.stop();
                }
                let wanted = audio_wanted.lock().ok().and_then(|wanted| wanted.clone());
                if let Some((output, phone)) = wanted {
                    match super::audio::AudioPlayback::start(&media_pipeline, pad, output, &phone) {
                        Ok(moved) => *playing = Some(moved),
                        Err(error) => eprintln!("[DeskLink] {error}"),
                    }
                }
            }
            return;
        }
        if !pad_carries_video(pad) {
            return;
        }
        match super::screen::attach_incoming_video(&media_pipeline, pad) {
            Ok(stream) => {
                let _ = media_events.send(PeerEvent::ScreenStream(stream));
            }
            Err(error) => {
                let _ = media_events.send(PeerEvent::Error(format!(
                    "Could not display the paired device's screen: {error}"
                )));
            }
        }
    });

    let state_events = events.clone();
    // `webrtcbin` emits state changes from its streaming/GLib worker.  The
    // `_local` variant wraps this closure in a ThreadGuard and aborts the
    // process when that worker invokes it.  The event sender is Send + Sync,
    // so use the cross-thread signal connection and forward only a string.
    webrtcbin.connect_notify(Some("connection-state"), move |element, _| {
        let state = element.property_value("connection-state");
        let _ = state_events.send(PeerEvent::ConnectionChanged(format!("{state:?}")));
    });
    Ok(())
}

fn install_data_channel(
    channel: WebRTCDataChannel,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
) -> Result<(), String> {
    let label = channel
        .label()
        .map(|label| label.to_string())
        .ok_or_else(|| "DeskLink WebRTC data channel has no label".to_string())?;
    let channel_kind = WebRtcChannel::parse(&label)?;
    if channel.is_ordered() != channel_kind.ordered()
        || channel.max_retransmits() != channel_kind.max_retransmits().map(i32::from).unwrap_or(-1)
    {
        channel.close();
        return Err(format!(
            "Invalid DeskLink WebRTC reliability settings for {label}"
        ));
    }
    {
        let mut channels = channels
            .lock()
            .map_err(|_| "DeskLink WebRTC channel lock poisoned".to_string())?;
        // Check before inserting. Inserting first displaced the working channel
        // and then closed the *new* one, so a duplicate label left the map
        // holding a closed entry and permanently broke that channel.
        if channels.contains_key(&channel_kind) {
            channel.close();
            return Err(format!("Duplicate DeskLink WebRTC data channel: {label}"));
        }
        channels.insert(channel_kind, channel.clone());
    }

    let open_events = events.clone();
    channel.connect_on_open(move |_| {
        let _ = open_events.send(PeerEvent::ChannelOpened(channel_kind));
    });
    let backlog = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_backlog = if channel_kind == WebRtcChannel::FileData {
        MAX_BACKLOG_FILE_BYTES
    } else {
        MAX_BACKLOG_BYTES
    };
    let message_events = events.clone();
    let message_backlog = Arc::clone(&backlog);
    channel.connect_on_message_string(move |_, data| {
        let Some(data) = data else {
            let _ = message_events.send(PeerEvent::Error(
                "DeskLink WebRTC received an empty text message".to_string(),
            ));
            return;
        };
        if data.len() > MAX_ENVELOPE_BYTES {
            let _ = message_events.send(PeerEvent::Error(
                "DeskLink WebRTC received an oversized message".to_string(),
            ));
            return;
        }
        let Some(credit) = take_credit(&message_backlog, data.len(), max_backlog) else {
            return;
        };
        let _ = message_events.send(PeerEvent::Envelope {
            channel: channel_kind,
            bytes: data.as_bytes().to_vec(),
            credit,
        });
    });
    let binary_events = events.clone();
    channel.connect_on_message_data(move |_, data| {
        let Some(data) = data else {
            let _ = binary_events.send(PeerEvent::Error(
                "DeskLink WebRTC received an empty binary message".to_string(),
            ));
            return;
        };
        let bytes = data.as_ref();
        if channel_kind != WebRtcChannel::FileData {
            let _ = binary_events.send(PeerEvent::Error(
                "DeskLink WebRTC rejected binary data on a non-file channel".to_string(),
            ));
            return;
        }
        if let Err(error) = validate_file_data(bytes) {
            let _ = binary_events.send(PeerEvent::Error(error));
            return;
        }
        let Some(credit) = take_credit(&backlog, bytes.len(), max_backlog) else {
            return;
        };
        let _ = binary_events.send(PeerEvent::Binary {
            channel: channel_kind,
            bytes: bytes.to_vec(),
            credit,
        });
    });
    let error_events = events;
    channel.connect_on_error(move |_, error| {
        let _ = error_events.send(PeerEvent::Error(format!(
            "DeskLink WebRTC data channel error: {error}"
        )));
    });
    Ok(())
}

fn request_local_description(
    webrtcbin: &gst::Element,
    message_type: SignalingMessageType,
    events: &Sender<PeerEvent>,
    offer_requested: &AtomicBool,
    force: bool,
) -> Result<(), String> {
    if matches!(message_type, SignalingMessageType::Offer)
        && !force
        && offer_requested.swap(true, Ordering::AcqRel)
    {
        return Ok(());
    }
    let webrtcbin_for_reply = webrtcbin.clone();
    let events_for_reply = events.clone();
    let description_field = local_description_promise_field(message_type)?;
    let promise = gst::Promise::with_change_func(move |reply| {
        let description = reply
            .ok()
            .and_then(|reply| reply)
            .and_then(|reply| reply.value(description_field).ok())
            .and_then(|value| value.get::<WebRTCSessionDescription>().ok());
        let Some(description) = description else {
            let _ = events_for_reply.send(PeerEvent::Error(
                "Could not create a local DeskLink WebRTC description".to_string(),
            ));
            return;
        };
        let sdp = match description.sdp().as_text() {
            Ok(sdp) => sdp.to_string(),
            Err(error) => {
                let _ = events_for_reply.send(PeerEvent::Error(format!(
                    "Could not serialize DeskLink WebRTC SDP: {error}"
                )));
                return;
            }
        };
        let events_after_set = events_for_reply.clone();
        let message_type_after_set = message_type;
        let set_promise = gst::Promise::with_change_func(move |reply| {
            if reply.is_err() {
                let _ = events_after_set.send(PeerEvent::Error(
                    "Could not set local DeskLink WebRTC description".to_string(),
                ));
            } else {
                let _ = events_after_set.send(PeerEvent::LocalDescription {
                    message_type: message_type_after_set,
                    sdp: without_video_ssrcs(&with_h264_receive_level(&sdp)),
                });
            }
        });
        webrtcbin_for_reply
            .emit_by_name::<()>("set-local-description", &[&description, &set_promise]);
    });
    let action = match message_type {
        SignalingMessageType::Offer => "create-offer",
        SignalingMessageType::Answer => "create-answer",
        _ => return Err("Invalid local DeskLink WebRTC description type".to_string()),
    };
    webrtcbin.emit_by_name::<()>(action, &[&None::<gst::Structure>, &promise]);
    Ok(())
}

/// GStreamer exposes a different reply field for `create-offer` and
/// `create-answer`. Reading `offer` for both silently loses every responder
/// answer and leaves the paired feature transport permanently gated.
fn local_description_promise_field(
    message_type: SignalingMessageType,
) -> Result<&'static str, String> {
    match message_type {
        SignalingMessageType::Offer => Ok("offer"),
        SignalingMessageType::Answer => Ok("answer"),
        _ => Err("Invalid local DeskLink WebRTC description type".to_string()),
    }
}

/// A candidate type in its short spelling: webrtcbin's stats say
/// "peer-reflexive", "server-reflexive" and "relayed", libwebrtc's SDP
/// "prflx", "srflx" and "relay" (contract B5).
fn candidate_kind(raw: &str) -> &str {
    match raw {
        "peer-reflexive" => "prflx",
        "server-reflexive" => "srflx",
        "relayed" => "relay",
        other => other,
    }
}

/// The connection-path rule, identical on both apps. `candidate_types` is
/// the selected pair's `[local, remote]` candidate types:
/// - either is `relay` → `"turn"`;
/// - both are `host`/`prflx` and the bootstrap link is not the relay → `"lan"`;
/// - anything else, including an unknown or incomplete pair → `"direct"`.
///
/// Either spelling of each type counts (see [`candidate_kind`]).
fn path_label(candidate_types: &[String], via_relay: bool) -> &'static str {
    let kinds: Vec<&str> = candidate_types
        .iter()
        .map(|kind| candidate_kind(kind))
        .collect();
    if kinds.contains(&"relay") {
        "turn"
    } else if !via_relay
        && kinds.len() == 2
        && kinds.iter().all(|kind| *kind == "host" || *kind == "prflx")
    {
        "lan"
    } else {
        "direct"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::mpsc;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn path_label_follows_the_shared_rule() {
        let label = |types: &[&str], via_relay| {
            let types: Vec<String> = types.iter().map(|kind| kind.to_string()).collect();
            path_label(&types, via_relay)
        };
        assert_eq!(label(&["relay", "host"], false), "turn");
        assert_eq!(label(&["host", "relay"], true), "turn");
        assert_eq!(label(&["host", "host"], false), "lan");
        assert_eq!(label(&["host", "prflx"], false), "lan");
        assert_eq!(label(&["prflx", "prflx"], false), "lan");
        assert_eq!(label(&["host", "host"], true), "direct");
        assert_eq!(label(&["host", "srflx"], false), "direct");
        assert_eq!(label(&["srflx", "srflx"], false), "direct");
        assert_eq!(label(&["host"], false), "direct");
        assert_eq!(label(&[], false), "direct");
        assert_eq!(label(&["", ""], false), "direct");
        // webrtcbin's own spellings (its stats).
        assert_eq!(label(&["host", "peer-reflexive"], false), "lan");
        assert_eq!(label(&["relayed", "host"], false), "turn");
        assert_eq!(label(&["host", "server-reflexive"], false), "direct");
    }

    /// What webrtcbin really reports for a connected pair, and that it
    /// reads as the LAN: two peers on loopback.
    #[test]
    fn a_connected_loopback_pair_reads_as_the_lan() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut path = None;
        while path.is_none() && std::time::Instant::now() < deadline {
            path = pair.offerer.path(false);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!(
            "[test] candidate types: {:?}",
            pair.offerer.selected_pair_types()
        );
        assert_eq!(path.map(|(label, _)| label), Some("lan"));
        pair.offerer.close();
        pair.answerer.close();
    }

    /// What libwebrtc (Android) offers when the phone is the initiator: one
    /// `sendrecv` VP8 video section — the phone attaches a track in both roles —
    /// plus the data-channel section. Hand-written from a real offer's shape so
    /// the desktop's *answerer* role can be exercised without a phone.
    const PHONE_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=extmap-allow-mixed\r\n\
a=msid-semantic: WMS desklink-screen\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:Zx4l\r\n\
a=ice-pwd:Ye1kU7eZ1cTnL7l3fJQ7t0q2\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 4B:1C:3E:8A:0D:52:97:6F:B3:21:E4:7A:C8:19:5D:F0:2A:66:B1:93:0E:D7:48:AC:35:7B:E2:19:84:CF:60:1D\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=extmap:1 urn:ietf:params:rtp-hdrext:toffset\r\n\
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
a=extmap:3 urn:3gpp:video-orientation\r\n\
a=extmap:4 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=sendrecv\r\n\
a=msid:desklink-screen 6f5d7f10-5c2e-4c0e-9a55-1f2e0b1d3c44\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtcp-fb:96 goog-remb\r\n\
a=rtcp-fb:96 transport-cc\r\n\
a=rtcp-fb:96 ccm fir\r\n\
a=rtcp-fb:96 nack\r\n\
a=rtcp-fb:96 nack pli\r\n\
a=rtpmap:97 rtx/90000\r\n\
a=fmtp:97 apt=96\r\n\
a=ssrc-group:FID 1111111111 2222222222\r\n\
a=ssrc:1111111111 cname:desklinkphone\r\n\
a=ssrc:1111111111 msid:desklink-screen 6f5d7f10-5c2e-4c0e-9a55-1f2e0b1d3c44\r\n\
a=ssrc:2222222222 cname:desklinkphone\r\n\
a=ssrc:2222222222 msid:desklink-screen 6f5d7f10-5c2e-4c0e-9a55-1f2e0b1d3c44\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:Zx4l\r\n\
a=ice-pwd:Ye1kU7eZ1cTnL7l3fJQ7t0q2\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 4B:1C:3E:8A:0D:52:97:6F:B3:21:E4:7A:C8:19:5D:F0:2A:66:B1:93:0E:D7:48:AC:35:7B:E2:19:84:CF:60:1D\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sctp-port:5000\r\n\
a=max-message-size:262144\r\n";

    /// Blocks until the peer produces its local answer.
    fn wait_for_answer(receiver: &mpsc::Receiver<PeerEvent>) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(remaining) {
                Ok(PeerEvent::LocalDescription {
                    message_type: SignalingMessageType::Answer,
                    sdp,
                }) => return sdp,
                Ok(PeerEvent::Error(error)) => panic!("negotiation failed: {error}"),
                Ok(_) => {}
                Err(_) => panic!("no answer was produced within 15s"),
            }
        }
    }

    /// An answerer with the phone's offer applied and answered — the state the
    /// desktop is in when a screen share starts.
    fn answered_peer(offer: &str) -> DesktopWebRtcPeer {
        let (sender, receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(false, sender, &IceServerConfig::default()).unwrap();
        peer.set_remote_description(SignalingMessageType::Offer, offer)
            .unwrap();
        wait_for_answer(&receiver);
        // Keep draining so the peer's event sender never blocks or errors.
        std::thread::spawn(move || while receiver.recv().is_ok() {});
        peer
    }

    fn test_source() -> gst::Element {
        gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("videotestsrc")
    }

    fn wait_for_frames(capture: &super::super::screen::DesktopScreenCapture) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && capture.frames_sent() == 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        capture.frames_sent()
    }

    /// The whole desktop video path except PipeWire: source → scale → VP8 → RTP
    /// → the section the phone's offer negotiated. This is the path that has
    /// never carried a frame; a chain that "attaches" to a fresh, unnegotiated
    /// transceiver reports success and sends nothing.
    #[test]
    fn the_screen_chain_binds_to_the_negotiated_section_and_produces_rtp() {
        let peer = answered_peer(PHONE_OFFER);

        let capture = super::super::screen::DesktopScreenCapture::attach_source(
            &peer.pipeline,
            &peer.webrtcbin,
            test_source(),
            None,
            1920,
            1080,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .expect("the chain must attach to the answered video section");

        assert!(
            wait_for_frames(&capture) > 0,
            "the encoder chain produced no RTP"
        );
        assert_eq!(capture.payload_type(), 96);
        // No connection in a unit test, so the chain parks inside `webrtcbin`:
        // `stop` must hand it back instead of hanging, and `close` frees it.
        assert!(capture.stop().is_err());
        peer.close();
    }

    /// libwebrtc hands out dynamic payload types; VP8 is not always 96. Sending
    /// with the wrong one is a stream the phone silently ignores.
    #[test]
    fn the_screen_chain_uses_the_payload_type_the_phone_offered() {
        let offer = PHONE_OFFER
            .replace("SAVPF 96 97", "SAVPF 100 101")
            .replace("a=rtpmap:96", "a=rtpmap:100")
            .replace("a=rtcp-fb:96", "a=rtcp-fb:100")
            .replace("a=rtpmap:97", "a=rtpmap:101")
            .replace("a=fmtp:97 apt=96", "a=fmtp:101 apt=100");
        let peer = answered_peer(&offer);

        let capture = super::super::screen::DesktopScreenCapture::attach_source(
            &peer.pipeline,
            &peer.webrtcbin,
            test_source(),
            None,
            1280,
            720,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .unwrap();

        assert_eq!(capture.payload_type(), 100);
        let _ = capture.stop();
        peer.close();
    }

    // ---- Two real peers connected over loopback --------------------------------
    //
    // Both ends are desktop code, so this proves the desktop's own transport, not
    // cross-stack interop. But it is real ICE, DTLS and SCTP on real `webrtcbin`s,
    // which is what the single-peer tests above cannot reach: with a connected
    // transport nothing parks, so this is where the *connected* shutdown path and
    // steady-state media flow are actually exercised.

    /// The far side builds a decode chain for the incoming track and frames
    /// actually decode and reach its display — a track alone is not a picture.
    fn assert_far_side_decodes(
        events: &mpsc::Receiver<PeerEvent>,
    ) -> super::super::screen::ScreenStream {
        // The far side reports either a working decode chain or *why* it could
        // not build one; silence would mean nothing arrived at all.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut outcome = None;
        while outcome.is_none() && std::time::Instant::now() < deadline {
            match events.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(PeerEvent::ScreenStream(stream)) => outcome = Some(Ok(stream)),
                Ok(PeerEvent::Error(error)) if error.contains("screen") => {
                    outcome = Some(Err(error))
                }
                _ => {}
            }
        }
        match outcome {
            Some(Ok(stream)) => {
                // A track is not a picture: the frames must decode and reach
                // the display, through the same codec negotiation as a phone.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while stream.frames_shown() < 5 && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                assert!(
                    stream.frames_shown() >= 5,
                    "the far side decoded {} frames",
                    stream.frames_shown()
                );
                stream
            }
            Some(Err(error)) => panic!("far side got the track but could not display it: {error}"),
            None => panic!("the far side never received the video track"),
        }
    }

    /// A capture that ends (here: a source that stops after a few frames; on a
    /// desktop, a PipeWire node that went away) must not EOS webrtcbin's shared
    /// video sink: that ended the screen track, and the next share on the same
    /// connection never reached the far side's picture.
    #[test]
    fn an_ending_capture_never_sends_eos_into_webrtcbin() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let pair = loopback();
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let watch_eos = |pad: &gst::Pad| {
            let seen = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&seen);
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    if event.type_() == gst::EventType::Eos {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                gst::PadProbeReturn::Ok
            });
            seen
        };
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 5i32)
            .build()
            .unwrap();
        let ended = watch_eos(&source.static_pad("src").unwrap());
        let share = |source| {
            super::super::screen::DesktopScreenCapture::attach_source(
                &pair.answerer.pipeline,
                &pair.answerer.webrtcbin,
                source,
                None,
                640,
                360,
                super::super::screen::ScreenRequest::FALLBACK,
                Some(super::super::screen::RtpContinuity {
                    ssrc: pair.answerer.screen_ssrc,
                    next_seqnum: None,
                }),
            )
            .unwrap()
        };
        let first = share(source);
        let reached: Vec<_> = pair
            .answerer
            .webrtcbin
            .sink_pads()
            .iter()
            .map(watch_eos)
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ended.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(ended.load(Ordering::SeqCst), "the first capture ended");
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            !reached.iter().any(|seen| seen.load(Ordering::SeqCst)),
            "EOS reached webrtcbin's video sink"
        );
        let _ = first.stop();
        pair.offerer.close();
        pair.answerer.close();
    }

    /// Reopening Remote input starts a second share on the same connection.
    /// It must reach the picture the far side already shows: with a fresh
    /// SSRC per share it was a stranger to a phone that had bound its picture
    /// to the first one, and the phone sat on "waiting for the first picture".
    #[test]
    fn a_second_share_on_the_same_connection_reaches_the_same_picture() {
        let pair = loopback();
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let share = |next_seqnum| {
            super::super::screen::DesktopScreenCapture::attach_source(
                &pair.answerer.pipeline,
                &pair.answerer.webrtcbin,
                test_source(),
                None,
                1280,
                720,
                super::super::screen::ScreenRequest::FALLBACK,
                Some(super::super::screen::RtpContinuity {
                    ssrc: pair.answerer.screen_ssrc,
                    next_seqnum,
                }),
            )
            .unwrap()
        };
        let first = share(None);
        let stream = assert_far_side_decodes(&pair.offerer_events);
        let next = first.next_seqnum();
        assert!(
            next.is_some(),
            "a share that sent packets knows where to continue"
        );
        assert!(first.stop().is_ok());

        let before = stream.frames_shown();
        let second = share(next);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while stream.frames_shown() < before + 10 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            stream.frames_shown() >= before + 10,
            "the second share never reached the far side's picture ({} -> {})",
            before,
            stream.frames_shown()
        );
        assert!(second.stop().is_ok());
        pair.offerer.close();
        pair.answerer.close();
    }

    /// A capture that fails to start (as a PipeWire stream that would not
    /// negotiate did) must leave the screen track usable: the next share
    /// reaches the far side's picture.
    #[test]
    fn a_failed_capture_leaves_the_screen_track_for_the_next_share() {
        let pair = loopback();
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let share = |source| {
            super::super::screen::DesktopScreenCapture::attach_source(
                &pair.answerer.pipeline,
                &pair.answerer.webrtcbin,
                source,
                None,
                1280,
                720,
                super::super::screen::ScreenRequest::FALLBACK,
                Some(super::super::screen::RtpContinuity {
                    ssrc: pair.answerer.screen_ssrc,
                    next_seqnum: None,
                }),
            )
        };
        // Cannot start: its file does not exist.
        let broken = gst::ElementFactory::make("filesrc")
            .property("location", "/nonexistent/desklink-screen")
            .build()
            .unwrap();
        assert!(share(broken).is_err());
        let working = share(test_source()).unwrap();
        let stream = assert_far_side_decodes(&pair.offerer_events);
        assert!(stream.frames_shown() > 0);
        assert!(working.stop().is_ok());
        pair.offerer.close();
        pair.answerer.close();
    }

    pub(crate) struct Loopback {
        pub(crate) offerer: Arc<DesktopWebRtcPeer>,
        pub(crate) answerer: Arc<DesktopWebRtcPeer>,
        pub(crate) offerer_events: mpsc::Receiver<PeerEvent>,
        pub(crate) answerer_events: mpsc::Receiver<PeerEvent>,
        _watchdog: Watchdog,
    }

    /// How long a loopback test may take before the run is stopped. Tests
    /// take seconds; this is for a GStreamer deadlock (a state change
    /// waiting on a blocked streaming thread), which otherwise hangs the
    /// whole suite with nothing to say which test.
    const LOOPBACK_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Ends the test run, naming the test, if it outlives its limit; the
    /// test ending (the loopback dropped) disarms it.
    struct Watchdog(Arc<std::sync::atomic::AtomicBool>);

    impl Watchdog {
        fn start(limit: std::time::Duration) -> Self {
            use std::sync::atomic::Ordering;
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let test = std::thread::current()
                .name()
                .unwrap_or("a loopback test")
                .to_string();
            let watching = Arc::clone(&done);
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + limit;
                while std::time::Instant::now() < deadline {
                    if watching.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                eprintln!("{test} hung for {limit:?}: stopping the test run");
                std::process::abort();
            });
            Self(done)
        }
    }

    impl Drop for Watchdog {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Wires two peers together: signalling events from each are applied to the
    /// other, everything else is handed to the test.
    pub(crate) fn loopback() -> Loopback {
        loopback_with(true, |_, sdp| sdp.to_string())
    }

    /// A loopback whose descriptions pass through `munge` on the way across
    /// (to stand in for an older phone), and whose offerer adds the audio
    /// section only with `offer_audio`.
    pub(crate) fn loopback_with(
        offer_audio: bool,
        munge: fn(SignalingMessageType, &str) -> String,
    ) -> Loopback {
        let watchdog = Watchdog::start(LOOPBACK_LIMIT);
        let ice = IceServerConfig {
            local_ips: vec![std::net::IpAddr::from([127, 0, 0, 1])],
            ..IceServerConfig::default()
        };
        let (offerer_tx, offerer_raw) = mpsc::channel();
        let (answerer_tx, answerer_raw) = mpsc::channel();
        NO_AUDIO_SECTION.with(|flag| flag.set(!offer_audio));
        let offerer = DesktopWebRtcPeer::new(true, offerer_tx, &ice);
        NO_AUDIO_SECTION.with(|flag| flag.set(false));
        let offerer = Arc::new(offerer.unwrap());
        let answerer = Arc::new(DesktopWebRtcPeer::new(false, answerer_tx, &ice).unwrap());
        let (offerer_app_tx, offerer_events) = mpsc::channel();
        let (answerer_app_tx, answerer_events) = mpsc::channel();

        let to = Arc::clone(&answerer);
        std::thread::spawn(move || {
            while let Ok(event) = offerer_raw.recv() {
                if std::env::var("LOOPBACK_TRACE").is_ok() {
                    debug_log!("OFFERER event: {:?}", event);
                }
                match event {
                    PeerEvent::LocalDescription { message_type, sdp } => {
                        let _ = to.set_remote_description(message_type, &munge(message_type, &sdp));
                    }
                    PeerEvent::IceCandidate {
                        sdp_m_line_index,
                        candidate,
                    } => {
                        let _ = to.add_ice_candidate(sdp_m_line_index, &candidate);
                    }
                    PeerEvent::EndOfCandidates => to.end_of_candidates(0),
                    other => {
                        let _ = offerer_app_tx.send(other);
                    }
                }
            }
        });
        let to = Arc::clone(&offerer);
        std::thread::spawn(move || {
            while let Ok(event) = answerer_raw.recv() {
                if std::env::var("LOOPBACK_TRACE").is_ok() {
                    debug_log!("ANSWERER event: {:?}", event);
                }
                match event {
                    PeerEvent::LocalDescription { message_type, sdp } => {
                        let _ = to.set_remote_description(message_type, &munge(message_type, &sdp));
                    }
                    PeerEvent::IceCandidate {
                        sdp_m_line_index,
                        candidate,
                    } => {
                        let _ = to.add_ice_candidate(sdp_m_line_index, &candidate);
                    }
                    PeerEvent::EndOfCandidates => to.end_of_candidates(0),
                    other => {
                        let _ = answerer_app_tx.send(other);
                    }
                }
            }
        });

        offerer.create_offer().unwrap();
        Loopback {
            offerer,
            answerer,
            offerer_events,
            answerer_events,
            _watchdog: watchdog,
        }
    }

    /// Waits until every channel in `channels` is open on the peer whose events
    /// are `events`. Open notifications arrive in any order, so waiting for them
    /// one at a time loses the ones that came early.
    pub(crate) fn wait_for_channels(
        events: &mpsc::Receiver<PeerEvent>,
        channels: &[WebRtcChannel],
    ) {
        let mut pending: Vec<WebRtcChannel> = channels.to_vec();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Ok(PeerEvent::ChannelOpened(opened)) => pending.retain(|c| *c != opened),
                Ok(PeerEvent::Error(error)) => panic!("peer error while connecting: {error}"),
                Ok(_) => {}
                Err(_) => panic!("{pending:?} never opened over loopback"),
            }
        }
    }

    /// Waits until `channel` is open on the peer whose events are `events`.
    pub(crate) fn wait_for_channel(events: &mpsc::Receiver<PeerEvent>, channel: WebRtcChannel) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Ok(PeerEvent::ChannelOpened(opened)) if opened == channel => return,
                Ok(PeerEvent::Error(error)) => panic!("peer error while connecting: {error}"),
                Ok(_) => {}
                Err(_) => panic!("{channel:?} never opened over loopback"),
            }
        }
    }

    /// Waits for the next `Envelope` on `channel`.
    fn next_envelope(events: &mpsc::Receiver<PeerEvent>, channel: WebRtcChannel) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Ok(PeerEvent::Envelope {
                    channel: got,
                    bytes,
                    ..
                }) if got == channel => return bytes,
                Ok(_) => {}
                Err(_) => panic!("no message arrived on {channel:?}"),
            }
        }
    }

    /// Stamped with the pipeline's running time, as `pipewiresrc` is in use
    /// (`do-timestamp`), and like it never waiting on the clock itself: a
    /// live `audiotestsrc` started on a running connection often sat in its
    /// own clock wait after two buffers, which `pipewiresrc` (pushed by
    /// PipeWire) never does. Bounded, so it cannot run on.
    fn audio_source() -> gst::Element {
        gst::ElementFactory::make("audiotestsrc")
            .property("is-live", false)
            .property("do-timestamp", true)
            .property("samplesperbuffer", 480i32)
            .property("num-buffers", 2_000i32)
            .build()
            .unwrap()
    }

    /// Waits until `peer` has an incoming audio pad, then plays it.
    fn play_when_audio_arrives(peer: &DesktopWebRtcPeer) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while peer.audio_pad.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "no audio pad arrived");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        peer.start_audio_playback(super::super::audio::Output::Speakers, "test")
            .unwrap();
    }

    fn wait_for_audio(peer: &DesktopWebRtcPeer) -> u64 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let received = peer.audio_received();
            if received > 20 || std::time::Instant::now() > deadline {
                return received;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The phone's `audio-ready` comes before its first packet: playback
    /// asked for then starts when the stream arrives.
    #[test]
    fn sound_asked_for_before_it_arrives_plays_when_it_does() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        assert!(pair.answerer.audio_pad.lock().unwrap().is_none());
        pair.answerer
            .start_audio_playback(super::super::audio::Output::Speakers, "test")
            .unwrap();
        pair.offerer
            .start_audio_send_from(super::super::audio::Source::System, audio_source())
            .unwrap();
        assert!(wait_for_audio(&pair.answerer) > 20, "nothing played");
    }

    /// F1 latency over loopback: each RTP packet's time from the payloader
    /// to the far side's webrtcbin output (SRTP, ICE, jitter buffer). Run by
    /// hand: `cargo test sound_latency -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn sound_latency_over_loopback() {
        use std::collections::HashMap;
        use std::time::Instant;
        type Stamps = Arc<Mutex<HashMap<u16, Instant>>>;
        fn seqnum(buffer: &gst::BufferRef) -> Option<u16> {
            let map = buffer.map_readable().ok()?;
            (map.len() > 4).then(|| u16::from_be_bytes([map[2], map[3]]))
        }
        fn stamp(pad: &gst::Pad, stamps: Stamps) {
            pad.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let now = Instant::now();
                    let mut stamps = stamps.lock().unwrap();
                    match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => {
                            if let Some(seq) = seqnum(buffer) {
                                stamps.entry(seq).or_insert(now);
                            }
                        }
                        Some(gst::PadProbeData::BufferList(list)) => {
                            for buffer in list.iter() {
                                if let Some(seq) = seqnum(buffer) {
                                    stamps.entry(seq).or_insert(now);
                                }
                            }
                        }
                        _ => {}
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let ticks = audio_source();
        ticks.set_property_from_str("wave", "ticks");
        pair.offerer
            .start_audio_send_from(super::super::audio::Source::System, ticks)
            .unwrap();
        play_when_audio_arrives(&pair.answerer);
        let (sent, heard): (Stamps, Stamps) = Default::default();
        let sink = pair
            .offerer
            .webrtcbin
            .sink_pads()
            .into_iter()
            .find(|pad| {
                pad.peer()
                    .and_then(|peer| peer.parent_element())
                    .and_then(|element| element.factory())
                    .is_some_and(|factory| factory.name() == "rtpopuspay")
            })
            .unwrap();
        stamp(&sink, Arc::clone(&sent));
        stamp(
            &pair.answerer.audio_pad.lock().unwrap().clone().unwrap(),
            Arc::clone(&heard),
        );
        std::thread::sleep(std::time::Duration::from_secs(5));
        let (sent, heard) = (sent.lock().unwrap(), heard.lock().unwrap());
        let mut delays: Vec<f64> = heard
            .iter()
            .filter_map(|(seq, at)| Some(at.duration_since(*sent.get(seq)?).as_secs_f64() * 1e3))
            .collect();
        delays.sort_by(f64::total_cmp);
        assert!(delays.len() > 100, "only {} packets matched", delays.len());
        let at = |q: f64| delays[((delays.len() - 1) as f64 * q) as usize];
        eprintln!(
            "SOUND LATENCY {} packets: median {:.2} ms, p95 {:.2} ms, max {:.2} ms",
            delays.len(),
            at(0.5),
            at(0.95),
            at(1.0)
        );
    }

    /// F1 A1: one Opus section in the first offer, after video, answered
    /// sendrecv — and sound flows both ways over it.
    #[test]
    fn sound_flows_both_ways_over_the_first_negotiation() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let offer = pair
            .offerer
            .webrtcbin
            .property::<Option<WebRTCSessionDescription>>("local-description")
            .unwrap()
            .sdp()
            .as_text()
            .unwrap();
        let audio = media_section(&offer, "audio");
        assert!(
            audio.contains("OPUS/48000/2") || audio.contains("opus/48000/2"),
            "{audio}"
        );
        for fmtp in [
            "minptime=10",
            "useinbandfec=1",
            "stereo=1",
            "maxaveragebitrate=128000",
        ] {
            assert!(audio.contains(fmtp), "{fmtp} missing in:\n{audio}");
        }
        assert!(audio.contains("a=sendrecv"));
        assert!(
            offer.find("m=video").unwrap() < offer.find("m=audio").unwrap(),
            "audio after video"
        );
        let answer = pair
            .answerer
            .webrtcbin
            .property::<Option<WebRTCSessionDescription>>("local-description")
            .unwrap()
            .sdp()
            .as_text()
            .unwrap();
        assert!(
            media_section(&answer, "audio").contains("a=sendrecv"),
            "{answer}"
        );
        assert!(pair.offerer.audio_negotiated() && pair.answerer.audio_negotiated());

        // Offerer → answerer.
        pair.offerer
            .start_audio_send_from(super::super::audio::Source::System, audio_source())
            .unwrap();
        play_when_audio_arrives(&pair.answerer);
        assert!(
            wait_for_audio(&pair.answerer) > 20,
            "answerer heard nothing"
        );
        // Answerer → offerer, at the same time.
        pair.answerer
            .start_audio_send_from(super::super::audio::Source::Mic, audio_source())
            .unwrap();
        play_when_audio_arrives(&pair.offerer);
        assert!(wait_for_audio(&pair.offerer) > 20, "offerer heard nothing");
        assert!(pair.offerer.audio_sent() > 0 && pair.answerer.audio_sent() > 0);

        pair.offerer.close();
        pair.answerer.close();
    }

    /// A second sound session on the same connection reuses the section.
    ///
    /// Ignored: under load (the full suite on a busy machine, about one run
    /// in six) the second session's first push sits in the `clocksync`
    /// webrtcbin puts in front of the audio sink, although its `sync` reads
    /// false. Not reproduced alone; unknown whether `pipewiresrc` (PipeWire
    /// pushes, it never waits on the clock) can meet it. Run by hand:
    /// `cargo test second_sound_session -- --ignored`.
    #[test]
    #[ignore]
    fn a_second_sound_session_on_the_same_connection_is_heard() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        pair.offerer
            .start_audio_send_from(super::super::audio::Source::System, audio_source())
            .unwrap();
        play_when_audio_arrives(&pair.answerer);
        assert!(
            wait_for_audio(&pair.answerer) > 20,
            "the first session was not heard"
        );
        // Stopping and starting again reuses the same section.
        pair.offerer.stop_audio_send();
        pair.answerer.stop_audio_playback();
        pair.offerer
            .start_audio_send_from(super::super::audio::Source::System, audio_source())
            .unwrap();
        pair.answerer
            .start_audio_playback(super::super::audio::Output::Microphone, "test")
            .unwrap();
        let heard = wait_for_audio(&pair.answerer);
        let pad = pair.answerer.audio_pad.lock().unwrap().clone();
        assert!(
            heard > 20,
            "a second session heard {heard} packets: sent {}, playing {:?}, pad {:?} -> {:?}",
            pair.offerer.audio_sent(),
            pair.answerer.audio_playback(),
            pad.as_ref().map(|p| p.name()),
            pad.and_then(|p| p.peer())
                .and_then(|p| p.parent_element())
                .map(|e| e.name())
        );
        pair.offerer.close();
        pair.answerer.close();
    }

    /// An older phone answers the audio section rejected: everything else
    /// keeps working and sound is simply unavailable.
    #[test]
    fn an_answer_without_audio_leaves_the_rest_working() {
        fn reject_audio(kind: SignalingMessageType, sdp: &str) -> String {
            if kind != SignalingMessageType::Answer {
                return sdp.to_string();
            }
            // Rejected the way libwebrtc rejects: port 0, inactive, and out
            // of the BUNDLE group.
            let section = &sdp[sdp.find("m=audio").unwrap()..];
            let mid = section
                .lines()
                .find_map(|line| line.strip_prefix("a=mid:"))
                .unwrap()
                .trim()
                .to_string();
            let mut in_audio = false;
            sdp.split_inclusive('\n')
                .map(|line| {
                    if line.starts_with("a=group:BUNDLE") {
                        let kept: Vec<&str> = line
                            .trim_end()
                            .split(' ')
                            .filter(|part| *part != mid)
                            .collect();
                        return format!("{}\r\n", kept.join(" "));
                    }
                    if line.starts_with("m=") {
                        in_audio = line.starts_with("m=audio");
                        if in_audio {
                            let mut parts: Vec<&str> = line.splitn(3, ' ').collect();
                            parts[1] = "0";
                            return parts.join(" ");
                        }
                    }
                    if in_audio && line.starts_with("a=sendrecv") {
                        return line.replace("sendrecv", "inactive");
                    }
                    line.to_string()
                })
                .collect()
        }
        let pair = loopback_with(true, reject_audio);
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        pair.answerer
            .send_text(WebRtcChannel::Control, "still here")
            .unwrap();
        assert_eq!(
            next_envelope(&pair.offerer_events, WebRtcChannel::Control),
            b"still here"
        );
        assert!(!pair.offerer.audio_negotiated());
        assert!(pair
            .offerer
            .start_audio_send_from(super::super::audio::Source::System, audio_source())
            .is_err());
        pair.offerer.close();
        pair.answerer.close();
    }

    /// An older phone offers no audio section: the desk answers what it was
    /// offered, and the connection works.
    #[test]
    fn an_offer_without_audio_is_answered_and_works() {
        let pair = loopback_with(false, |_, sdp| sdp.to_string());
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        pair.offerer
            .send_text(WebRtcChannel::Control, "old phone")
            .unwrap();
        assert_eq!(
            next_envelope(&pair.answerer_events, WebRtcChannel::Control),
            b"old phone"
        );
        assert!(!pair.answerer.audio_negotiated());
        pair.offerer.close();
        pair.answerer.close();
    }

    #[test]
    fn two_peers_connect_over_loopback_and_carry_data_both_ways() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);

        pair.offerer
            .send_text(WebRtcChannel::Control, "offerer says hello")
            .unwrap();
        assert_eq!(
            next_envelope(&pair.answerer_events, WebRtcChannel::Control),
            b"offerer says hello"
        );
        pair.answerer
            .send_text(WebRtcChannel::Control, "answerer replies")
            .unwrap();
        assert_eq!(
            next_envelope(&pair.offerer_events, WebRtcChannel::Control),
            b"answerer replies"
        );

        pair.offerer.close();
        pair.answerer.close();
    }

    /// Screen video from the *answerer* — the role the desktop plays when the
    /// phone is the initiator — over a real connection: it keeps flowing (nothing
    /// is parked), and stopping the share is clean instead of handed back.
    #[test]
    fn the_answerer_streams_screen_video_over_a_live_connection_and_stops_cleanly() {
        let pair = loopback();
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);

        let capture = super::super::screen::DesktopScreenCapture::attach_source(
            &pair.answerer.pipeline,
            &pair.answerer.webrtcbin,
            test_source(),
            None,
            1280,
            720,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .expect("the answerer must be able to attach its screen to the negotiated section");

        assert!(wait_for_frames(&capture) > 0, "no video produced");
        let early = capture.frames_sent();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(
            capture.frames_sent() > early + 10,
            "video stalled once past the first packets (parked behind the transport?): {} -> {}",
            early,
            capture.frames_sent()
        );
        // The phone-is-initiator case: the offerer must see the picture.
        assert_far_side_decodes(&pair.offerer_events);

        assert!(
            capture.stop().is_ok(),
            "on a live connection nothing is parked, so stopping must complete"
        );
        pair.offerer.close();
        pair.answerer.close();
    }

    /// The other role: the desktop as *offerer* (its id sorts lower), streaming to
    /// the phone. The far side must actually receive it — a decode chain is built
    /// for the incoming track — not just see the sender report packets.
    #[test]
    fn the_offerer_streams_screen_video_and_the_far_side_starts_receiving_it() {
        let pair = loopback();
        wait_for_channel(&pair.offerer_events, WebRtcChannel::Control);

        let capture = super::super::screen::DesktopScreenCapture::attach_source(
            &pair.offerer.pipeline,
            &pair.offerer.webrtcbin,
            test_source(),
            None,
            1280,
            720,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .expect("the offerer must be able to attach its screen to its own video section");
        assert!(wait_for_frames(&capture) > 0, "no video produced");

        assert_far_side_decodes(&pair.answerer_events);

        assert!(
            capture.stop().is_ok(),
            "a live connection must stop cleanly"
        );
        pair.offerer.close();
        pair.answerer.close();
    }

    /// Closing a peer that is mid-share must return promptly, connected or not.
    #[test]
    fn closing_a_connected_peer_with_an_active_share_does_not_hang() {
        let pair = loopback();
        wait_for_channel(&pair.answerer_events, WebRtcChannel::Control);
        let _capture = super::super::screen::DesktopScreenCapture::attach_source(
            &pair.answerer.pipeline,
            &pair.answerer.webrtcbin,
            test_source(),
            None,
            1280,
            720,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let started = std::time::Instant::now();
        pair.answerer.close();
        pair.offerer.close();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "close took {:?}",
            started.elapsed()
        );
    }

    /// A chain nothing is flowing through has nothing parked in `webrtcbin`, so
    /// `stop` winds it down completely rather than handing it back.
    #[test]
    fn a_quiet_capture_stops_cleanly_and_leaves_the_peer_usable() {
        let peer = answered_peer(PHONE_OFFER);
        let silent = gst::ElementFactory::make("appsrc")
            .property("is-live", true)
            .build()
            .expect("appsrc");
        let capture = super::super::screen::DesktopScreenCapture::attach_source(
            &peer.pipeline,
            &peer.webrtcbin,
            silent,
            None,
            1280,
            720,
            super::super::screen::ScreenRequest::FALLBACK,
            None,
        )
        .unwrap();

        assert!(capture.stop().is_ok(), "an idle chain must stop cleanly");
        peer.close();
    }

    /// A second share on the same peer must reuse the section's pad and still
    /// carry media. (The first share here ends cleanly — an idle chain — because
    /// a chain left *parked* in a connection that never came up holds the pad's
    /// stream lock; a real share only starts on a ready connection, and a lost
    /// one rebuilds the peer.)
    #[test]
    fn a_second_share_reuses_the_same_section_and_carries_media() {
        let peer = answered_peer(PHONE_OFFER);
        let attach = |source: gst::Element| {
            super::super::screen::DesktopScreenCapture::attach_source(
                &peer.pipeline,
                &peer.webrtcbin,
                source,
                None,
                1280,
                720,
                super::super::screen::ScreenRequest::FALLBACK,
                None,
            )
            .unwrap()
        };
        let silent = || {
            gst::ElementFactory::make("appsrc")
                .property("is-live", true)
                .build()
                .expect("appsrc")
        };

        let first = attach(silent());
        assert!(first.stop().is_ok());

        let second = attach(test_source());
        assert!(
            wait_for_frames(&second) > 0,
            "the second share produced nothing"
        );
        let _parked = second.stop();
        peer.close();
    }

    /// The section of `sdp` that starts at the first line beginning `m=<kind>`.
    fn media_section<'a>(sdp: &'a str, kind: &str) -> &'a str {
        let start = sdp
            .find(&format!("m={kind} "))
            .unwrap_or_else(|| panic!("no m={kind} section in:\n{sdp}"));
        let rest = &sdp[start..];
        let end = rest[1..].find("\nm=").map_or(rest.len(), |i| i + 2);
        &rest[..end]
    }

    /// The paired phone (device id sorts lower) is the WebRTC initiator, so the
    /// desktop is the **answerer** — and it is the desktop that has to send the
    /// screen video. An answerer's transceiver is created by the remote offer,
    /// not by us; if it answers `recvonly` the phone gets an m-line that can
    /// never carry a frame, however healthy everything else looks.
    #[test]
    fn an_answerer_will_send_screen_video_when_the_phone_offers_sendrecv() {
        let (sender, receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(false, sender, &IceServerConfig::default()).unwrap();
        peer.set_remote_description(SignalingMessageType::Offer, PHONE_OFFER)
            .unwrap();

        let answer = wait_for_answer(&receiver);
        peer.close();

        let video = media_section(&answer, "video");
        assert!(
            video.contains("a=sendrecv"),
            "the desktop must answer sendrecv to be able to send its screen; answered:\n{video}"
        );
        assert!(
            video.contains("VP8/90000"),
            "the answer must keep the negotiated VP8 codec:\n{video}"
        );
        // Without TWCC the phone's congestion control is loss-only and its
        // pacer holds every packet ~100 ms; the offer's id must be echoed.
        assert!(
            video.contains(
                "a=extmap:4 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
            ),
            "the answer must negotiate transport-wide congestion control:\n{video}"
        );
    }

    #[test]
    fn the_phone_is_never_told_the_screen_streams_ssrc() {
        let sdp = "v=0\r\nm=video 9 X 100 96\r\na=mid:0\r\na=ssrc-group:FID 0 3880827048\r\n\
                   a=ssrc:3880827048 msid:user@host webrtctransceiver0\r\na=sendrecv\r\n\
                   m=application 9 Y webrtc-datachannel\r\na=ssrc:7 cname:keep\r\n";
        let out = without_video_ssrcs(sdp);
        assert!(!out.contains("a=ssrc-group"), "{out}");
        assert!(!out.contains("a=ssrc:3880827048"), "{out}");
        assert!(out.contains("a=sendrecv\r\n") && out.contains("a=mid:0\r\n"));
        // Other sections are untouched, line endings included.
        assert!(out.contains("a=ssrc:7 cname:keep\r\n"));
        assert_eq!(without_video_ssrcs("v=0\r\n"), "v=0\r\n");
    }

    #[test]
    fn the_desk_states_its_own_h264_receive_level() {
        let sdp = "v=0\r\nm=audio 9 X 111\r\na=fmtp:111 profile-level-id=42e01f\r\n\
                   m=video 9 X 102 96\r\na=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
                   a=fmtp:96 x-google-start-bitrate=800\r\n";
        let out = with_h264_receive_level(sdp);
        assert!(out.contains(
            "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e028\r\n"
        ));
        assert!(
            out.contains("a=fmtp:111 profile-level-id=42e01f"),
            "only the video section"
        );
        assert!(out.contains("a=fmtp:96 x-google-start-bitrate=800"));
    }

    /// `get-transceiver` takes a **`gint`**, and `emit_by_name` panics on a
    /// mismatched argument type — inside a GStreamer promise callback, where a
    /// panic aborts the process rather than surfacing.
    ///
    /// This shipped as `0u32..` and took the daemon down with "Incompatible
    /// argument type in argument 0 for signal 'get-transceiver' (expected gint,
    /// got guint)" the first time a remote description arrived. Nothing in the
    /// type system catches it: the values are marshalled dynamically. Only
    /// firing the signal for real does.
    #[test]
    fn configure_video_transceivers_marshals_its_index_to_a_real_webrtcbin() {
        let (sender, _receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(true, sender, &IceServerConfig::default()).unwrap();

        // The initiator has a video transceiver, so this walks a populated list
        // and then runs off the end — both paths the promise callback takes.
        configure_video_transceivers(&peer.webrtcbin);

        peer.close();
    }

    /// End-of-candidates is signalled by a **NULL** candidate, and
    /// `emit_by_name` panics on a signature it cannot marshal — so this is
    /// pinned against a real `webrtcbin` rather than assumed. The same class of
    /// mistake against the `ice-agent` property once killed the peer's event
    /// worker on every negotiation.
    #[test]
    fn end_of_candidates_marshals_a_null_candidate_to_a_real_webrtcbin() {
        let (sender, _receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(true, sender, &IceServerConfig::default()).unwrap();

        peer.end_of_candidates(0);

        // And the empty-string path stays rejected, so the two cannot be
        // confused: `add_ice_candidate` must never be the way this is sent.
        assert!(peer.add_ice_candidate(0, "").is_err());
        peer.close();
    }

    #[test]
    fn initiator_creates_each_cross_platform_channel_once() {
        let (sender, _receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(true, sender, &IceServerConfig::default()).unwrap();
        let channels = peer.channels.lock().unwrap();

        assert_eq!(channels.len(), WebRtcChannel::ALL.len());
        assert!(channels.contains_key(&WebRtcChannel::Control));
        assert!(channels.contains_key(&WebRtcChannel::InputRealtime));
        drop(channels);
        peer.close();
    }

    #[test]
    fn file_data_is_bounded_and_nonempty() {
        assert!(validate_file_data(&[1]).is_ok());
        assert!(validate_file_data(&[]).is_err());
        assert!(validate_file_data(&vec![0; MAX_ENVELOPE_BYTES + 1]).is_err());
    }
    #[test]
    fn answer_creation_reads_the_answer_promise_field() {
        assert_eq!(
            local_description_promise_field(SignalingMessageType::Answer).unwrap(),
            "answer"
        );
    }

    #[test]
    fn empty_local_ice_candidate_marks_gathering_complete() {
        assert!(matches!(
            local_ice_event(0, String::new()),
            PeerEvent::EndOfCandidates
        ));
    }
}
