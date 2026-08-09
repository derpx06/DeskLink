use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use gst::prelude::*;
use gst_webrtc::{
    WebRTCDataChannel, WebRTCDataChannelState, WebRTCSDPType, WebRTCSessionDescription,
};

use super::{SignalingMessageType, WebRtcChannel, MAX_ENVELOPE_BYTES};

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
    Binary {
        channel: WebRtcChannel,
        bytes: Vec<u8>,
    },
    ConnectionChanged(String),
    Error(String),
    Closed,
}

/// A single GStreamer `webrtcbin` peer. It owns no DeskLink feature state: it
/// merely exposes authenticated data-channel and negotiation events to the
/// session coordinator, which remains the sole owner of session validity.
pub struct DesktopWebRtcPeer {
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    initiator: bool,
    offer_requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

impl DesktopWebRtcPeer {
    pub fn new(initiator: bool, events: Sender<PeerEvent>) -> Result<Self, String> {
        ensure_gstreamer()?;
        let pipeline = gst::Pipeline::new();
        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .build()
            .map_err(|error| format!("DeskLink WebRTC is unavailable: {error}"))?;
        webrtcbin.set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);
        pipeline
            .add(&webrtcbin)
            .map_err(|error| format!("Could not build DeskLink WebRTC pipeline: {error}"))?;

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let offer_requested = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        install_webrtcbin_callbacks(
            &webrtcbin,
            Arc::clone(&channels),
            events.clone(),
            initiator,
            Arc::clone(&offer_requested),
            Arc::clone(&closed),
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
            closed,
        };
        if initiator {
            peer.create_local_channels()?;
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
        if let Ok(channels) = self.channels.lock() {
            for channel in channels.values() {
                channel.close();
            }
        }
        let promise = gst::Promise::new();
        self.webrtcbin.emit_by_name::<()>("close", &[&promise]);
        let _ = self.pipeline.set_state(gst::State::Null);
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
        .get_or_init(|| gst::init().map_err(|error| error.to_string()))
        .clone()
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

fn install_webrtcbin_callbacks(
    webrtcbin: &gst::Element,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    initiator: bool,
    offer_requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
) -> Result<(), String> {
    let negotiation_element = webrtcbin.clone();
    let negotiation_events = events.clone();
    let negotiation_requested = Arc::clone(&offer_requested);
    let negotiation_closed = Arc::clone(&closed);
    webrtcbin.connect("on-negotiation-needed", false, move |_| {
        if initiator && !negotiation_closed.load(Ordering::Acquire) {
            let _ = request_local_description(
                &negotiation_element,
                SignalingMessageType::Offer,
                &negotiation_events,
                &negotiation_requested,
                false,
            );
        }
        None
    });

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
        if channels.insert(channel_kind, channel.clone()).is_some() {
            channel.close();
            return Err(format!("Duplicate DeskLink WebRTC data channel: {label}"));
        }
    }

    let open_events = events.clone();
    channel.connect_on_open(move |_| {
        let _ = open_events.send(PeerEvent::ChannelOpened(channel_kind));
    });
    let message_events = events.clone();
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
        let _ = message_events.send(PeerEvent::Envelope {
            channel: channel_kind,
            bytes: data.as_bytes().to_vec(),
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
        let _ = binary_events.send(PeerEvent::Binary {
            channel: channel_kind,
            bytes: bytes.to_vec(),
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
                    sdp,
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn initiator_creates_each_cross_platform_channel_once() {
        let (sender, _receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new(true, sender).unwrap();
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
