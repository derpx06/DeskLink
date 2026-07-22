//! WebRTC transport contracts and implementation boundary.
//!
//! Feature handlers use the transport contract instead of depending on
//! `webrtcbin` or signaling details. The legacy LAN transport remains the
//! fallback until a WebRTC generation has authenticated successfully.

#![allow(dead_code)]

pub mod authentication;
pub mod channel;
pub mod envelope;
pub mod negotiation;
pub mod peer_connection;
pub mod signaling;
pub mod transport;

pub const TRANSPORT_ID_PREFIX: &str = "webrtc";
pub const MAX_ENVELOPE_BYTES: usize = 256 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;

#[cfg(test)]
mod tests {
    use super::channel::{ChannelKind, DataChannelSpec};

    #[test]
    fn channel_contract_is_stable() {
        let labels: Vec<_> = DataChannelSpec::all()
            .iter()
            .map(|channel| channel.label)
            .collect();
        assert_eq!(labels.len(), 7);
        assert!(labels.contains(&"desklink-control-v1"));
        assert!(labels.contains(&"desklink-input-realtime-v1"));
        assert_eq!(
            DataChannelSpec::for_label("desklink-input-realtime-v1")
                .expect("realtime channel")
                .kind,
            ChannelKind::Realtime
        );
    }
}
