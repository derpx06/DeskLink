//! `webrtcbin` signal handlers: inbound pads by media, ICE candidates, data
//! channels and their messages, and the local description request.
//!
//! Sibling edges: `rtp_tuning`, `video_sdp`. Outside: `screen`, `audio`,
//! `file_protocol`.

use super::*;

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

#[allow(clippy::too_many_arguments)] // Captures the peer's shared state once.
pub(super) fn install_webrtcbin_callbacks(
    webrtcbin: &gst::Element,
    pipeline: &gst::Pipeline,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    audio_pad: Arc<Mutex<Option<gst::Pad>>>,
    audio_playback: Arc<Mutex<Option<crate::device_links::webrtc::audio::AudioPlayback>>>,
    audio_wanted: Arc<Mutex<Option<(crate::device_links::webrtc::audio::Output, String)>>>,
    held: Arc<Mutex<Held>>,
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
    let incoming_held = Arc::clone(&held);
    let incoming_events = events.clone();
    webrtcbin.connect("on-data-channel", false, move |values| {
        match values[1].get::<WebRTCDataChannel>() {
            Ok(channel) => {
                let _ = install_data_channel(
                    channel,
                    Arc::clone(&incoming_channels),
                    incoming_events.clone(),
                    Arc::clone(&incoming_held),
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
    //
    // The pipeline owns `webrtcbin`, which owns this closure: holding the
    // pipeline here made a reference cycle that `close()` never broke, so every
    // replaced peer kept its whole pipeline, ICE agent, sockets and threads for
    // the life of the process. The closure holds it weakly.
    let media_pipeline = pipeline.downgrade();
    webrtcbin.connect_pad_added(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let Some(media_pipeline) = media_pipeline.upgrade() else {
            return; // the peer is gone
        };
        // Sound (F1): parked until someone listens, never left unlinked.
        if pad_carries_audio(pad) {
            crate::device_links::webrtc::screen::park_pad(&media_pipeline, pad);
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
                    match crate::device_links::webrtc::audio::AudioPlayback::start(
                        &media_pipeline,
                        pad,
                        output,
                        &phone,
                    ) {
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
        match crate::device_links::webrtc::screen::attach_incoming_video(&media_pipeline, pad) {
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
        let state = element.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        if state == gst_webrtc::WebRTCPeerConnectionState::Connected {
            // Off this thread: the stats are waited for, and this
            // callback must never wait on anything.
            let webrtcbin = element.downgrade();
            std::thread::spawn(move || {
                let port = webrtcbin
                    .upgrade()
                    .and_then(|webrtcbin| selected_local_port(&webrtcbin))?;
                let size = widen_receive_buffer(port);
                debug_log!("[DeskLink] ICE socket :{port} receive buffer: {size:?} bytes");
                Some(())
            });
        }
        let state = element.property_value("connection-state");
        let _ = state_events.send(PeerEvent::ConnectionChanged(format!("{state:?}")));
    });
    Ok(())
}

/// The callbacks set here run on the streaming thread that `close()` waits for
/// (`shut_inbound_data`): they must never block, and only send on an
/// unbounded channel.
pub(super) fn install_data_channel(
    channel: WebRTCDataChannel,
    channels: Arc<Mutex<HashMap<WebRtcChannel, WebRTCDataChannel>>>,
    events: Sender<PeerEvent>,
    held: Arc<Mutex<Held>>,
) -> Result<(), String> {
    let label = channel
        .label()
        .map(|label| label.to_string())
        .ok_or_else(|| "DeskLink WebRTC data channel has no label".to_string())?;
    let channel_kind = WebRtcChannel::parse(&label)?;
    if channel.is_ordered() != channel_kind.ordered()
        || channel.max_retransmits() != channel_kind.max_retransmits().map(i32::from).unwrap_or(-1)
    {
        // Not closed: closing takes the stream's `sctpdec` pad away while the
        // far side may still be sending on it, and `sctpdec` aborts the
        // process on data for a stream with no pad (see `shut_inbound_data`).
        // Nothing is connected to it, so what it sends is dropped.
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
            // Left open and unconnected, for the reason above.
            return Err(format!("Duplicate DeskLink WebRTC data channel: {label}"));
        }
        channels.insert(channel_kind, channel.clone());
    }

    if channel_kind == WebRtcChannel::FileData {
        // A sender waiting for room is woken when the buffer drains.
        let wake_held = Arc::clone(&held);
        channel.set_buffered_amount_low_threshold(
            crate::device_links::webrtc::file_protocol::IN_FLIGHT_LOW,
        );
        channel.connect_on_buffered_amount_low(move |_| {
            let wake = wake_held
                .lock()
                .ok()
                .and_then(|held| held.file_wake.clone());
            if let Some(FileWake(wake)) = wake {
                wake();
            }
        });
    }
    if channel_kind == WebRtcChannel::Events {
        // Held sends resume, in order, once the queue drains.
        channel.set_buffered_amount_low_threshold(EVENTS_LOW_WATER);
        channel.connect_on_buffered_amount_low(move |channel| {
            let Ok(mut held) = held.lock() else {
                return;
            };
            while let Some(text) = held.next(channel.property::<u64>("buffered-amount")) {
                if channel.send_string_full(Some(&text)).is_err() {
                    break;
                }
            }
        });
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
        // File data is binary frames only; text on it is not a file message.
        if channel_kind == WebRtcChannel::FileData {
            let _ = message_events.send(PeerEvent::Error(
                "DeskLink WebRTC rejected text on the file-data channel".to_string(),
            ));
            return;
        }
        let Some(data) = data else {
            let _ = message_events.send(PeerEvent::Error(
                "DeskLink WebRTC received an empty text message".to_string(),
            ));
            return;
        };
        if data.len() > channel_kind.max_inbound_bytes() {
            // Dropped and logged; the channel stays.
            let _ = message_events.send(PeerEvent::Error(format!(
                "DeskLink WebRTC received an oversized message on {}",
                channel_kind.label()
            )));
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

pub(super) fn request_local_description(
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
pub(super) fn local_description_promise_field(
    message_type: SignalingMessageType,
) -> Result<&'static str, String> {
    match message_type {
        SignalingMessageType::Offer => Ok("offer"),
        SignalingMessageType::Answer => Ok("answer"),
        _ => Err("Invalid local DeskLink WebRTC description type".to_string()),
    }
}
