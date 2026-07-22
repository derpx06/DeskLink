use std::collections::HashMap;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_webrtc::{WebRTCDataChannel, WebRTCDataChannelState};

use super::channel::{ChannelError, DataChannelSpec};
use super::envelope::MessageEnvelope;

/// Thin, GLib-thread-owned wrapper around the desktop `webrtcbin` element.
///
/// SDP and ICE messages are deliberately delivered to the signaling adapter by
/// the caller. This keeps the peer connection independent of whether the
/// signal travels over the existing LAN link or a configured WSS endpoint.
pub struct PeerConnection {
    element: gst::Element,
    channels: HashMap<String, WebRTCDataChannel>,
}

impl PeerConnection {
    pub fn new(element: gst::Element) -> Self {
        Self {
            element,
            channels: HashMap::new(),
        }
    }

    pub fn element(&self) -> &gst::Element {
        &self.element
    }

    pub fn create_channels(&mut self) -> Result<(), ChannelError> {
        for spec in DataChannelSpec::all() {
            let mut options =
                gst::Structure::builder("desklink-data-channel").field("ordered", spec.ordered);
            if let Some(max_retransmits) = spec.max_retransmits {
                options = options.field("max-retransmits", i32::from(max_retransmits));
            }
            if let Some(channel) = self.element.emit_by_name::<Option<WebRTCDataChannel>>(
                "create-data-channel",
                &[&spec.label, &Some(options.build())],
            ) {
                self.channels.insert(spec.label.to_string(), channel);
            }
        }
        Ok(())
    }

    pub fn channel_state(&self, label: &str) -> Option<WebRTCDataChannelState> {
        self.channels.get(label).map(WebRTCDataChannel::ready_state)
    }

    pub fn send(&self, label: &str, payload: &[u8]) -> Result<(), String> {
        let channel = self
            .channels
            .get(label)
            .ok_or_else(|| format!("unknown WebRTC data channel: {label}"))?;
        if channel.ready_state() != WebRTCDataChannelState::Open {
            return Err(format!("WebRTC data channel is not open: {label}"));
        }
        channel
            .send_data_full(Some(&gst::glib::Bytes::from(payload)))
            .map_err(|error| error.to_string())
    }

    pub fn send_envelope(
        &self,
        channel: &DataChannelSpec,
        envelope: &MessageEnvelope,
    ) -> Result<(), String> {
        if envelope.channel != channel.label {
            return Err(format!(
                "WebRTC envelope channel does not match data channel: {}",
                channel.label
            ));
        }
        let bytes = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
        self.send(channel.label, &bytes)
    }

    pub fn attach_message_handler<F>(&self, label: &str, handler: F) -> Result<(), String>
    where
        F: Fn(&[u8]) + Send + Sync + 'static,
    {
        let channel = self
            .channels
            .get(label)
            .ok_or_else(|| format!("unknown WebRTC data channel: {label}"))?;
        channel.connect_on_message_data(move |_, bytes| {
            if let Some(bytes) = bytes {
                handler(bytes.as_ref());
            }
        });
        Ok(())
    }

    pub fn close(&self) {
        for channel in self.channels.values() {
            channel.close();
        }
        let _ = self.element.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webrtcbin_is_available_on_this_runtime() {
        gst::init().unwrap();
        let element = gst::ElementFactory::make("webrtcbin")
            .name("desklink-test-webrtcbin")
            .build()
            .expect("GStreamer WebRTC plugin is required by desktop builds");
        let mut peer = PeerConnection::new(element);
        // Data channels are created after the negotiated peer is attached to
        // a running pipeline. The plugin itself must still be available at
        // construction time.
        peer.create_channels().unwrap();
        peer.close();
    }
}
