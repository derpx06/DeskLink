//! WebRTC transport contracts and implementation boundary.
//!
//! Feature handlers use the transport contract instead of depending on
//! `webrtcbin` or signaling details. The LAN transport is retained only for
//! discovery, pairing, identity exchange, and signed WebRTC signaling.

#![allow(dead_code)]

pub mod authentication;
pub mod channel;
pub mod envelope;
pub mod file_browser;
pub mod file_protocol;
pub mod file_transfer;
pub mod handover;
pub mod negotiation;
pub mod packet_bridge;
pub mod peer_connection;
pub mod recovery;
pub mod signaling;
pub mod transport;
pub mod wire_binding;

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
