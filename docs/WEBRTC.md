# DeskLink WebRTC transport

This branch adds the first WebRTC transport foundation while preserving the
existing authenticated LAN transport as the active fallback. WebRTC is not
selected for a device until signaling, transcript authentication, channel
negotiation, and generation handover have completed successfully.

## Contract

- One peer connection belongs to one logical `DeviceSession`.
- Every message carries the device ID, logical session ID, transport
  generation, unique message ID, message type, timestamp, and bounded payload.
- Unknown channel labels, wrong reliability settings, stale generations,
  unsupported envelope versions, malformed payloads, and oversized payloads are
  rejected before dispatch.
- The exact channels are defined in
  `src/device_links/webrtc/channel.rs` and mirrored by
  `desklink-mobile/src/main/java/org/desklink/mobile/webrtc/WebRtcChannel.kt`.
- `desklink.webrtc.signal.v1` is a signaling packet identifier only. It is not
  advertised as a feature capability until the negotiated transport is wired
  into the session manager.

## Signaling

`InMemorySignalingTransport` is used by deterministic unit tests. The local LAN
adapter serializes signaling through `desklink.webrtc.signal.v1`. The WSS
client accepts only an explicitly configured `wss://` endpoint; no endpoint,
account provider, server hostname, or TURN credential is shipped by DeskLink.

The signaling service is expected to route signed offer, answer, ICE, ICE
restart, end-of-candidates, and close messages. It must not be treated as the
peer trust authority; the existing paired identity keys authenticate the
transcript after the control channel is established.

## Runtime dependencies

The Linux adapter uses the installed GStreamer 1.28 WebRTC plugin through the
Rust `gstreamer`, `gstreamer-webrtc`, `gstreamer-video`, and `gstreamer-app`
crates. If `webrtcbin` is unavailable, the LAN transport remains available and
the UI must report WebRTC as unavailable rather than silently changing identity
or pairing state.

The Android adapter uses `io.github.webrtc-sdk:android:144.7559.05`. See the
mobile dependency record for its license, ABI list, and artifact checksum.

## Validation status

Unit coverage exists for channel validation, envelope limits and binding,
signaling replay/destination checks, P-256 transcript signatures, GStreamer
plugin discovery, and Android envelope parsing. Same-LAN peer negotiation,
different-network ICE, external WSS signaling, TURN, and physical-device
feature migration remain integration gates and are not claimed by unit tests.
