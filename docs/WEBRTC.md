# DeskLink WebRTC transport

This branch performs same-LAN WebRTC peer negotiation while preserving the
existing authenticated LAN transport as the active fallback. The peer is
created only after pairing, and SDP/ICE travels over the existing
certificate-pinned LAN control link.

## Contract

- One peer connection belongs to one logical `DeviceSession`.
- `DeviceManager` is the only owner allowed to install, replace, or clear the
  WebRTC transport for a session generation; replacement closes the old peer.
- Every message carries the device ID, logical session ID, transport
  generation, unique message ID, message type, timestamp, and bounded payload.
- Unknown channel labels, wrong reliability settings, stale generations,
  unsupported envelope versions, malformed payloads, and oversized payloads are
  rejected before dispatch.
- The exact channels are defined in
  `src/device_links/webrtc/channel.rs` and mirrored by
  `desklink-mobile/src/main/java/org/desklink/mobile/webrtc/WebRtcChannel.kt`.
- `desklink.webrtc.signal.v1` is an opt-in paired-session control capability.
  It is rejected before pairing and is never routed through plugin handlers.

## Signaling

`InMemorySignalingTransport` is used by deterministic unit tests. The local LAN
adapter serializes signaling through `desklink.webrtc.signal.v1`. The WSS
client accepts only an explicitly configured `wss://` endpoint; no endpoint,
account provider, server hostname, or TURN credential is shipped by DeskLink.

Every LAN signaling message is signed by the existing paired device identity
key and verified against the certificate-pinned remote public key. It also
binds source/destination IDs, timestamp, request ID, attempt ID, SDP or ICE
payload, and message type. Replayed, expired, stale-session, unsigned, or
wrong-device messages are rejected.

The optional WSS client remains a future signaling transport. It is not used
by the same-LAN integration test and it is not a peer trust authority.

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
cross-platform canonical signaling records, replay/destination checks,
identity-key signatures, GStreamer plugin discovery, and Android envelope
parsing.

## Same-LAN verification

1. Install current builds on the desktop and Android phone. Pair them first.
2. On Linux, enable the opt-in setting and restart DeskLink so it advertises
   the capability:

   ```bash
   desklink preferences set webrtc.enabled true
   ```

3. On Android, open **Settings**, enable **Experimental WebRTC transport**,
   then force-stop/reopen DeskLink. Restarting both applications is simplest.
4. Watch the desktop state signal before the devices reconnect:

   ```bash
   busctl --user monitor derx06.desklink.com | grep -E 'FeatureStateChanged|webrtc'
   ```

   The successful event contains `feature=webrtc` and `state=Ready`. The
   desktop process also prints `[WebRTC] device=… state=Ready`.
5. On Android, confirm the matching status:

   ```bash
   adb logcat -s DeskLink/WebRTC
   ```

   It must report `Ready (Peer connection and control data channel are open…)`.

`Ready` proves that an offer, answer, trickle ICE, DTLS, and the fixed control
data channel completed between the actual desktop and phone. It does **not**
yet prove that clipboard, transfers, notifications, input, or screen sharing
are running over WebRTC: those features deliberately remain on the verified
LAN fallback until their per-feature handover tests are added.

Different-network, external WSS, TURN, ICE restart, media tracks, and feature
handover remain integration gates and are not claimed by this stage.
