//! Relay protocol v1: JSON text frames tagged by `"t"`.
//!
//! These types are public so the desktop client can reuse them instead of
//! re-typing the wire format. The relay never interprets `data`; clients put
//! their own end-to-end encrypted envelope there.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest client version still accepted. Raising it closes older clients with
/// [`CLOSE_UPGRADE_REQUIRED`] instead of letting them misread newer frames.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Domain-separation prefix of the signed auth string. It keeps a relay
/// challenge signature from being replayable as any other DeskLink signature
/// made with the same device key.
pub const AUTH_CONTEXT: &str = "desklink-relay-auth-v1";

/// A frame was not valid protocol v1 (bad JSON, unknown type, wrong phase,
/// oversized allow list).
pub const CLOSE_BAD_REQUEST: u16 = 4400;
/// Authentication failed or did not arrive within 10 s.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
/// A newer connection authenticated with the same certificate.
pub const CLOSE_REPLACED: u16 = 4409;
/// The client's protocol version is older than [`MIN_PROTOCOL_VERSION`].
pub const CLOSE_UPGRADE_REQUIRED: u16 = 4426;
/// A rate, connection or byte limit was exceeded.
pub const CLOSE_RATE_LIMITED: u16 = 4429;

/// Most entries one `allow` frame may carry. A device only ever needs its
/// paired peers here; the cap bounds per-connection hub memory.
pub const MAX_ALLOW: usize = 32;

/// The exact bytes a client signs to answer a challenge.
///
/// The relay's public host name is part of the input so a signature collected
/// by one relay (or a malicious one) cannot be replayed against another.
pub fn auth_sign_input(nonce: &str, public_host: &str) -> Vec<u8> {
    format!("{AUTH_CONTEXT}\n{nonce}\n{public_host}").into_bytes()
}

/// Unknown fields are ignored so a newer client can add optional fields
/// without a version bump; unknown `t` values are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ClientMessage {
    Auth {
        v: u32,
        /// Base64 (standard alphabet) DER X.509 certificate.
        cert: String,
        /// Base64 (standard alphabet) signature over [`auth_sign_input`].
        sig: String,
    },
    /// Replaces the sender's allow set.
    Allow {
        fps: Vec<String>,
    },
    Send {
        to: String,
        data: Value,
    },
    Turn {
        peer: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ServerMessage {
    Challenge {
        v: u32,
        #[serde(rename = "minV")]
        min_v: u32,
        /// Base64url (no padding) of 32 random bytes.
        nonce: String,
    },
    Welcome {
        fp: String,
    },
    Presence {
        fp: String,
        online: bool,
    },
    Msg {
        /// Stamped by the relay from the authenticated certificate, never
        /// copied from the sender's frame.
        from: String,
        data: Value,
    },
    TurnCreds {
        peer: String,
        #[serde(rename = "iceServers")]
        ice_servers: Vec<IceServer>,
        ttl: u64,
    },
    Error {
        code: ErrorCode,
        to: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// The peer is not reachable *for this sender*. Also returned when the
    /// peer is online but has not allowed the sender, so a device that is not
    /// mutual can never tell "offline" from "ignoring me".
    Offline,
    /// The sender's own allow set does not contain the target. This reveals
    /// nothing the sender did not already know.
    NotAllowed,
    /// Not mutual, peer offline, no TURN backend, or the backend failed.
    TurnDenied,
    /// The pair or the network used up today's TURN grants.
    TurnQuota,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip_server(message: ServerMessage, wire: Value) {
        assert_eq!(serde_json::to_value(&message).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ServerMessage>(wire).unwrap(),
            message
        );
    }

    fn round_trip_client(message: ClientMessage, wire: Value) {
        assert_eq!(serde_json::to_value(&message).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ClientMessage>(wire).unwrap(),
            message
        );
    }

    #[test]
    fn server_messages_use_the_documented_wire_shapes() {
        round_trip_server(
            ServerMessage::Challenge {
                v: 1,
                min_v: 1,
                nonce: "n".into(),
            },
            json!({"t": "challenge", "v": 1, "minV": 1, "nonce": "n"}),
        );
        round_trip_server(
            ServerMessage::Welcome { fp: "f".into() },
            json!({"t": "welcome", "fp": "f"}),
        );
        round_trip_server(
            ServerMessage::Presence {
                fp: "f".into(),
                online: true,
            },
            json!({"t": "presence", "fp": "f", "online": true}),
        );
        round_trip_server(
            ServerMessage::Msg {
                from: "f".into(),
                data: json!("opaque"),
            },
            json!({"t": "msg", "from": "f", "data": "opaque"}),
        );
        round_trip_server(
            ServerMessage::TurnCreds {
                peer: "p".into(),
                ice_servers: vec![IceServer {
                    urls: vec!["turn:h:3478?transport=udp".into()],
                    username: Some("u".into()),
                    credential: Some("c".into()),
                }],
                ttl: 60,
            },
            json!({"t": "turnCreds", "peer": "p", "ttl": 60, "iceServers": [
                {"urls": ["turn:h:3478?transport=udp"], "username": "u", "credential": "c"}
            ]}),
        );
        for (code, text) in [
            (ErrorCode::Offline, "offline"),
            (ErrorCode::NotAllowed, "not-allowed"),
            (ErrorCode::TurnDenied, "turn-denied"),
            (ErrorCode::TurnQuota, "turn-quota"),
        ] {
            round_trip_server(
                ServerMessage::Error {
                    code,
                    to: "p".into(),
                },
                json!({"t": "error", "code": text, "to": "p"}),
            );
        }
    }

    #[test]
    fn client_messages_use_the_documented_wire_shapes() {
        round_trip_client(
            ClientMessage::Auth {
                v: 1,
                cert: "c".into(),
                sig: "s".into(),
            },
            json!({"t": "auth", "v": 1, "cert": "c", "sig": "s"}),
        );
        round_trip_client(
            ClientMessage::Allow {
                fps: vec!["a".into(), "b".into()],
            },
            json!({"t": "allow", "fps": ["a", "b"]}),
        );
        round_trip_client(
            ClientMessage::Send {
                to: "p".into(),
                data: json!({"any": ["json", 1]}),
            },
            json!({"t": "send", "to": "p", "data": {"any": ["json", 1]}}),
        );
        round_trip_client(
            ClientMessage::Turn { peer: "p".into() },
            json!({"t": "turn", "peer": "p"}),
        );
    }

    #[test]
    fn unknown_types_are_rejected_and_unknown_fields_ignored() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"t":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"peer":"p"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<ClientMessage>(r#"{"t":"turn","peer":"p","x":1}"#).unwrap(),
            ClientMessage::Turn { peer: "p".into() }
        );
    }

    #[test]
    fn sign_input_is_context_nonce_and_host_separated_by_newlines() {
        assert_eq!(
            auth_sign_input("NONCE", "relay.example.com"),
            b"desklink-relay-auth-v1\nNONCE\nrelay.example.com".to_vec()
        );
    }
}
