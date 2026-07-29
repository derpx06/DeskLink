use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use gst::prelude::*;
use gst_app::prelude::*;
use gst_webrtc::{
    WebRTCDataChannel, WebRTCDataChannelState, WebRTCSDPType, WebRTCSessionDescription,
};
use uuid::Uuid;

use super::portal::PortalScreenCapture;
use super::video_receive::{self, RemoteVideoFrame};
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
    device_id: String,
    video_token: String,
    pipeline: gst::Pipeline,
    webrtcbin: gst::Element,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    initiator: bool,
    offer_requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    connection_state: Arc<Mutex<String>>,
    disconnected_grace_scheduled: Arc<AtomicBool>,
    video_sender: Arc<Mutex<PreparedVideoSender>>,
}

impl DesktopWebRtcPeer {
    pub fn new(
        device_id: String,
        initiator: bool,
        events: Sender<PeerEvent>,
    ) -> Result<Self, String> {
        ensure_gstreamer()?;
        let pipeline = gst::Pipeline::new();
        let webrtcbin = gst::ElementFactory::make("webrtcbin")
            .build()
            .map_err(|error| format!("DeskLink WebRTC is unavailable: {error}"))?;
        webrtcbin.set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);
        pipeline
            .add(&webrtcbin)
            .map_err(|error| format!("Could not build DeskLink WebRTC pipeline: {error}"))?;
        // Reserve a single VP8 sender before the first SDP offer. It has no
        // source until a user explicitly starts desktop sharing, so the peer
        // has a stable media section without a second feature handover or a
        // renegotiation caused by each view/control switch.
        let video_sender = PreparedVideoSender::install(&pipeline, &webrtcbin)?;

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let offer_requested = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let connection_state = Arc::new(Mutex::new("New".to_string()));
        let disconnected_grace_scheduled = Arc::new(AtomicBool::new(false));
        let video_token = Uuid::new_v4().to_string();
        // Creating the first local data channel emits `on-negotiation-needed`.
        // Do not let that callback race the remaining channel creation: an
        // offer created mid-setup has no SCTP `m=application` section.
        let negotiation_enabled = Arc::new(AtomicBool::new(false));
        install_webrtcbin_callbacks(
            &webrtcbin,
            &pipeline,
            device_id.clone(),
            video_token.clone(),
            Arc::clone(&channels),
            events.clone(),
            initiator,
            Arc::clone(&offer_requested),
            Arc::clone(&closed),
            Arc::clone(&negotiation_enabled),
            Arc::clone(&connection_state),
            Arc::clone(&disconnected_grace_scheduled),
        )?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| format!("Could not start DeskLink WebRTC pipeline: {error:?}"))?;

        let peer = Self {
            device_id,
            video_token,
            pipeline,
            webrtcbin,
            channels,
            events,
            initiator,
            offer_requested,
            closed,
            connection_state,
            disconnected_grace_scheduled,
            video_sender: Arc::new(Mutex::new(video_sender)),
        };
        if initiator {
            peer.create_local_channels()?;
            negotiation_enabled.store(true, Ordering::Release);
        }
        Ok(peer)
    }

    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    /// Arms at most one grace timer for a transient ICE `Disconnected` state.
    /// A later Connected/Failed/Closed callback clears the arm, so an old
    /// timer can never rebuild a healthy replacement peer.
    pub fn arm_disconnected_grace(&self) -> bool {
        self.connection_state()
            .contains("disconnected")
            && !self
                .disconnected_grace_scheduled
                .swap(true, Ordering::AcqRel)
    }

    pub fn is_still_disconnected(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.connection_state().contains("disconnected")
    }

    fn connection_state(&self) -> String {
        self.connection_state
            .lock()
            .map(|state| state.to_ascii_lowercase())
            .unwrap_or_default()
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

    /// Starts an already portal-authorized desktop capture on the VP8 media
    /// section that was present in the initial SDP. The PipeWire descriptor is
    /// retained by the capture graph until it is stopped, so the portal never
    /// closes the remote while GStreamer is still reading it.
    pub fn start_desktop_capture(&self, capture: PortalScreenCapture) -> Result<(), String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("DeskLink WebRTC peer is closed".to_string());
        }
        self.video_sender
            .lock()
            .map_err(|_| "DeskLink desktop video sender lock poisoned".to_string())?
            .start(capture)
    }

    pub fn stop_desktop_capture(&self) {
        if let Ok(mut sender) = self.video_sender.lock() {
            sender.stop();
        }
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
        self.stop_desktop_capture();
        let promise = gst::Promise::new();
        self.webrtcbin.emit_by_name::<()>("close", &[&promise]);
        let _ = self.pipeline.set_state(gst::State::Null);
        video_receive::clear_if_current(&self.device_id, &self.video_token);
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

/// The permanently negotiated outbound VP8 media section. It uses an
/// `input-selector` with no active source until a ScreenCast portal session
/// provides PipeWire frames, preventing dynamic pad creation from causing an
/// unverified renegotiation while the user is working.
struct PreparedVideoSender {
    pipeline: gst::Pipeline,
    selector: gst::Element,
    webrtcbin: gst::Element,
    webrtc_sink_pad: gst::Pad,
    capture: Option<DesktopCaptureGraph>,
}

struct DesktopCaptureGraph {
    elements: Vec<gst::Element>,
    selector_sink_pad: gst::Pad,
    // GStreamer receives only a raw descriptor property. Owning this fd keeps
    // the portal-provided PipeWire remote alive for the full capture lifetime.
    _pipewire_remote: OwnedFd,
}

impl PreparedVideoSender {
    fn install(pipeline: &gst::Pipeline, webrtcbin: &gst::Element) -> Result<Self, String> {
        let selector = make_element("input-selector", "desklink-video-selector")?;
        let queue = make_element("queue", "desklink-video-send-queue")?;
        let convert = make_element("videoconvert", "desklink-video-send-convert")?;
        let scale = make_element("videoscale", "desklink-video-send-scale")?;
        let rate = make_element("videorate", "desklink-video-send-rate")?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("desklink-video-send-caps")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(15, 1))
                    .build(),
            )
            .build()
            .map_err(|error| format!("Could not create DeskLink video caps filter: {error}"))?;
        let encoder = make_element("vp8enc", "desklink-vp8-encoder")?;
        let payloader = gst::ElementFactory::make("rtpvp8pay")
            .name("desklink-vp8-payloader")
            .property("pt", 96u32)
            .build()
            .map_err(|error| format!("Could not create DeskLink VP8 payloader: {error}"))?;

        pipeline
            .add_many([
                &selector,
                &queue,
                &convert,
                &scale,
                &rate,
                &capsfilter,
                &encoder,
                &payloader,
            ])
            .map_err(|error| format!("Could not add DeskLink screen sender: {error}"))?;
        gst::Element::link_many([
            &selector,
            &queue,
            &convert,
            &scale,
            &rate,
            &capsfilter,
            &encoder,
            &payloader,
        ])
        .map_err(|error| format!("Could not link DeskLink screen sender: {error}"))?;
        let webrtc_sink_pad = webrtcbin
            .request_pad_simple("sink_%u")
            .ok_or_else(|| "Could not reserve the DeskLink VP8 WebRTC sender pad".to_string())?;
        payloader
            .static_pad("src")
            .ok_or_else(|| "DeskLink VP8 payloader has no source pad".to_string())?
            .link(&webrtc_sink_pad)
            .map_err(|error| format!("Could not connect DeskLink VP8 to WebRTC: {error}"))?;

        Ok(Self {
            pipeline: pipeline.clone(),
            selector,
            webrtcbin: webrtcbin.clone(),
            webrtc_sink_pad,
            capture: None,
        })
    }

    fn start(&mut self, capture: PortalScreenCapture) -> Result<(), String> {
        self.stop();

        let source = gst::ElementFactory::make("pipewiresrc")
            .name("desklink-portal-screen-source")
            .property("fd", capture.pipewire_remote.as_raw_fd())
            .property("path", capture.node_id.to_string())
            .property("do-timestamp", true)
            .build()
            .map_err(|error| format!("PipeWire screen capture is unavailable: {error}"))?;
        let queue = make_element("queue", "desklink-portal-screen-queue")?;
        let convert = make_element("videoconvert", "desklink-portal-screen-convert")?;
        let scale = make_element("videoscale", "desklink-portal-screen-scale")?;
        let rate = make_element("videorate", "desklink-portal-screen-rate")?;
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name("desklink-portal-screen-caps")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(15, 1))
                    .build(),
            )
            .build()
            .map_err(|error| format!("Could not create DeskLink capture caps filter: {error}"))?;
        let elements = vec![source, queue, convert, scale, rate, capsfilter];
        self.pipeline
            .add_many(elements.iter().collect::<Vec<_>>())
            .map_err(|error| format!("Could not add DeskLink desktop capture: {error}"))?;
        gst::Element::link_many(elements.iter().collect::<Vec<_>>())
            .map_err(|error| format!("Could not link DeskLink desktop capture: {error}"))?;
        let selector_sink_pad = self
            .selector
            .request_pad_simple("sink_%u")
            .ok_or_else(|| "Could not reserve the DeskLink desktop-capture input".to_string())?;
        let capture_src_pad = elements
            .last()
            .and_then(|element| element.static_pad("src"))
            .ok_or_else(|| "DeskLink desktop capture has no source pad".to_string())?;
        capture_src_pad
            .link(&selector_sink_pad)
            .map_err(|error| format!("Could not select DeskLink desktop capture: {error}"))?;
        self.selector.set_property("active-pad", &selector_sink_pad);
        for element in &elements {
            element
                .sync_state_with_parent()
                .map_err(|error| format!("Could not start DeskLink desktop capture: {error}"))?;
        }
        self.capture = Some(DesktopCaptureGraph {
            elements,
            selector_sink_pad,
            _pipewire_remote: capture.pipewire_remote,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        if let Some(source) = capture.elements.last().and_then(|element| element.static_pad("src")) {
            let _ = source.unlink(&capture.selector_sink_pad);
        }
        self.selector.release_request_pad(&capture.selector_sink_pad);
        for element in capture.elements.iter().rev() {
            let _ = element.set_state(gst::State::Null);
            let _ = self.pipeline.remove(element);
        }
    }
}

impl Drop for PreparedVideoSender {
    fn drop(&mut self) {
        self.stop();
        self.webrtcbin.release_request_pad(&self.webrtc_sink_pad);
    }
}

fn make_element(factory: &str, name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|error| format!("Could not create DeskLink {factory}: {error}"))
}

fn validate_file_data(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err("DeskLink WebRTC file-data chunk is outside the allowed size".to_string());
    }
    Ok(())
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
    pipeline: &gst::Pipeline,
    device_id: String,
    video_token: String,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    initiator: bool,
    offer_requested: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    negotiation_enabled: Arc<AtomicBool>,
    connection_state: Arc<Mutex<String>>,
    disconnected_grace_scheduled: Arc<AtomicBool>,
) -> Result<(), String> {
    let negotiation_element = webrtcbin.clone();
    let negotiation_events = events.clone();
    let negotiation_requested = Arc::clone(&offer_requested);
    let negotiation_closed = Arc::clone(&closed);
    let negotiation_is_enabled = Arc::clone(&negotiation_enabled);
    webrtcbin.connect("on-negotiation-needed", false, move |_| {
        if initiator
            && negotiation_is_enabled.load(Ordering::Acquire)
            && !negotiation_closed.load(Ordering::Acquire)
        {
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
        if candidate.is_empty() || candidate.len() > 16 * 1024 {
            let _ = ice_events.send(PeerEvent::Error(
                "Received an invalid local DeskLink WebRTC ICE candidate".to_string(),
            ));
        } else {
            let _ = ice_events.send(PeerEvent::IceCandidate {
                sdp_m_line_index,
                candidate,
            });
        }
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

    install_vp8_receive_path(webrtcbin, pipeline, device_id, video_token, events.clone())?;

    let state_events = events.clone();
    webrtcbin.connect_notify_local(Some("connection-state"), move |element, _| {
        let state = element.property_value("connection-state");
        let state = format!("{state:?}");
        if let Ok(mut current) = connection_state.lock() {
            *current = state.clone();
        }
        if !state.to_ascii_lowercase().contains("disconnected") {
            disconnected_grace_scheduled.store(false, Ordering::Release);
        }
        let _ = state_events.send(PeerEvent::ConnectionChanged(state));
    });
    Ok(())
}

/// Links the incoming Android VP8 track to a bounded RGBA frame queue.  GTK
/// turns those frames into textures on its own main thread; no GTK object is
/// ever sent through the GStreamer/WebRTC worker channels.
fn install_vp8_receive_path(
    webrtcbin: &gst::Element,
    pipeline: &gst::Pipeline,
    device_id: String,
    video_token: String,
    events: Sender<PeerEvent>,
) -> Result<(), String> {
    let (frame_sender, frame_receiver) = mpsc::sync_channel(2);
    video_receive::install(device_id, video_token, frame_receiver);
    let pipeline = pipeline.clone();
    webrtcbin.connect_pad_added(move |_element, pad| {
        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        let Some(caps) = caps else {
            let _ = events.send(PeerEvent::Error(
                "DeskLink WebRTC video pad has no negotiated caps".to_string(),
            ));
            return;
        };
        let Some(structure) = caps.structure(0) else {
            return;
        };
        if structure.name() != "application/x-rtp"
            || structure
                .get::<String>("media")
                .ok()
                .as_deref()
                != Some("video")
        {
            return;
        }
        if structure
            .get::<String>("encoding-name")
            .ok()
            .as_deref()
            != Some("VP8")
        {
            let _ = events.send(PeerEvent::Error(
                "DeskLink received an unsupported remote screen codec; VP8 is required"
                    .to_string(),
            ));
            return;
        }

        let result = (|| -> Result<(), String> {
            let queue = gst::ElementFactory::make("queue").build().map_err(|error| error.to_string())?;
            let depay = gst::ElementFactory::make("rtpvp8depay").build().map_err(|error| error.to_string())?;
            let decode = gst::ElementFactory::make("vp8dec").build().map_err(|error| error.to_string())?;
            let convert = gst::ElementFactory::make("videoconvert").build().map_err(|error| error.to_string())?;
            let capsfilter = gst::ElementFactory::make("capsfilter")
                .property("caps", gst::Caps::builder("video/x-raw").field("format", "RGBA").build())
                .build()
                .map_err(|error| error.to_string())?;
            let sink = gst::ElementFactory::make("appsink")
                .property("max-buffers", 2u32)
                .property("drop", true)
                .property("sync", false)
                .build()
                .map_err(|error| error.to_string())?
                .downcast::<gst_app::AppSink>()
                .map_err(|_| "Could not create the DeskLink VP8 frame sink".to_string())?;
            install_frame_callback(&sink, frame_sender.clone());
            pipeline
                .add_many([&queue, &depay, &decode, &convert, &capsfilter, sink.upcast_ref()])
                .map_err(|error| error.to_string())?;
            gst::Element::link_many([&queue, &depay, &decode, &convert, &capsfilter, sink.upcast_ref()])
                .map_err(|error| error.to_string())?;
            pad.link(&queue.static_pad("sink").ok_or_else(|| "VP8 queue has no sink pad".to_string())?)
                .map_err(|error| error.to_string())?;
            for element in [&queue, &depay, &decode, &convert, &capsfilter, sink.upcast_ref()] {
                element.sync_state_with_parent().map_err(|error| error.to_string())?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = events.send(PeerEvent::Error(format!(
                "Could not start DeskLink VP8 screen receiver: {error}"
            )));
        }
    });
    Ok(())
}

fn install_frame_callback(sink: &gst_app::AppSink, sender: mpsc::SyncSender<RemoteVideoFrame>) {
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                let info = gst_video::VideoInfo::from_caps(caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let stride = usize::try_from(info.stride()[0]).map_err(|_| gst::FlowError::Error)?;
                video_receive::publish(
                    &sender,
                    RemoteVideoFrame {
                        width: i32::try_from(info.width()).map_err(|_| gst::FlowError::Error)?,
                        height: i32::try_from(info.height()).map_err(|_| gst::FlowError::Error)?,
                        stride,
                        rgba: map.as_slice().to_vec(),
                    },
                );
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
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
    let description_field = match message_type {
        SignalingMessageType::Offer => "offer",
        SignalingMessageType::Answer => "answer",
        _ => return Err("Invalid local DeskLink WebRTC description type".to_string()),
    };
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
        _ => unreachable!("message type was validated above"),
    };
    webrtcbin.emit_by_name::<()>(action, &[&None::<gst::Structure>, &promise]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn initiator_creates_each_cross_platform_channel_once() {
        let (sender, _receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new("test-device".to_string(), true, sender).unwrap();
        let channels = peer.channels.lock().unwrap();

        assert_eq!(channels.len(), WebRtcChannel::ALL.len());
        assert!(channels.contains_key(&WebRtcChannel::Control));
        assert!(channels.contains_key(&WebRtcChannel::InputRealtime));
        drop(channels);
        peer.close();
    }

    #[test]
    fn initiator_creates_a_local_offer_for_its_data_channels() {
        let (sender, receiver) = mpsc::channel();
        let peer = DesktopWebRtcPeer::new("test-device".to_string(), true, sender).unwrap();
        peer.create_offer().unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match receiver.try_recv() {
                Ok(PeerEvent::LocalDescription {
                    message_type: SignalingMessageType::Offer,
                    sdp,
                }) => {
                    assert!(
                        sdp.contains("m=application"),
                        "offer has no WebRTC data-channel media section:\n{sdp}"
                    );
                    peer.close();
                    return;
                }
                Ok(PeerEvent::Error(error)) => panic!("could not create local offer: {error}"),
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("WebRTC peer stopped before creating an offer")
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the local WebRTC offer"
            );
            while gst::glib::MainContext::default().pending() {
                gst::glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn file_data_is_bounded_and_nonempty() {
        assert!(validate_file_data(&[1]).is_ok());
        assert!(validate_file_data(&[]).is_err());
        assert!(validate_file_data(&vec![0; MAX_ENVELOPE_BYTES + 1]).is_err());
    }

}
