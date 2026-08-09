# DeskLink Android–Desktop WebRTC Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a stable, authenticated WebRTC-only paired-feature connection between DeskLink Android and the GNOME desktop app.

**Architecture:** LAN/TLS is limited to discovery, identity, pairing, and signed SDP/ICE signaling. `DeviceManager` owns one generation-checked peer per device. Feature-ready is enabled only after the desktop registry, file manager, input lease, and screen track lifecycle are integrated and validated.

**Tech Stack:** Rust, GTK4/libadwaita, GStreamer `webrtcbin`, XDG portals, PipeWire, D-Bus; Kotlin/Java, Android WebRTC, MediaProjection, AccessibilityService, SAF.

## Global constraints

- Modify only `DeskLink` and `desklink-mobile`; never modify `kdeconnect-kde`.
- Preserve DeskLink branding, application IDs, pairing records, keys, certificates, ports, and configuration paths.
- Existing DeskLink v9 packet bodies remain inside authenticated WebRTC envelopes.
- No paired-feature LAN fallback after mutual `feature-ready`.
- No relay deployment, WebRTC audio, remote shell, Wi-Fi Direct, or shared-library work.
- Preserve the three existing Android user edits until they are explicitly integrated and tested.

## Task 1 — Desktop capability registry and shared dispatcher

**Files:**
- Create `DeskLink/src/device_links/features/{mod.rs,registry.rs}`.
- Modify `DeskLink/src/device_links/{device_info.rs,daemon/dispatcher.rs,daemon/packet_handler.rs}`.
- Test `DeskLink/src/device_links/features/registry.rs`.

**Interfaces:**

```rust
pub trait FeatureHandler: Send + Sync {
    fn packet_types(&self) -> &'static [&'static str];
    fn incoming_available(&self) -> bool;
    fn outgoing_available(&self) -> bool;
    fn handle(&self, context: &FeatureContext, packet: NetworkPacket) -> Result<(), FeatureError>;
}
```

- [ ] Write failing tests: unavailable handlers do not appear in capabilities; unregistered packets are rejected; a supported packet has one handler.
- [ ] Run `cargo test feature_registry -- --test-threads=1`; expect failure because the registry does not exist.
- [ ] Move existing ping, clipboard, battery, notification update/cancel, media state, volume state, command state, and find-device receive effects into explicit handlers.
- [ ] Make `DeviceInfo::local` derive both capability lists from the registry, not the static broad list.
- [ ] Preserve identity, pair, and `desklink.webrtc.signal.v1` as bootstrap-only—not feature handlers.
- [ ] Run registry plus dispatcher tests; commit `feat(desktop): derive capabilities from active handlers`.

## Task 2 — Mutual feature-ready cutover

**Files:**
- Modify `DeskLink/src/device_links/{webrtc/coordinator.rs,daemon/packet_handler.rs,daemon/commands.rs}`.
- Modify `desklink-mobile/src/main/java/org/desklink/mobile/{Device.kt,webrtc/WebRtcSessionCoordinator.kt}`.
- Test both WebRTC coordinator test suites.

- [ ] Write failing Rust/Kotlin tests for `DTLS -> authenticated -> capabilities -> both feature-ready`; assert Android `setWebRtcTransport()` occurs only at the last transition.
- [ ] Implement desktop local `FeatureReady` transmission after the signed remote capability list exactly matches active handlers.
- [ ] Route every ordinary WebRTC envelope through the shared registry, then reject LAN feature packets after cutover.
- [ ] Change post-cutover sends to return visible `WebRtcUnavailable` errors; remove ordinary feature use of bootstrap TLS.
- [ ] Test stale generation, replay, wrong channel, malformed packet, wrong direction, unknown capability, and LAN rejection after cutover.
- [ ] Run `cargo test webrtc daemon::dispatcher -- --test-threads=1` and `./gradlew testDebugUnitTest`; commit `feat: activate WebRTC feature handover`.

## Task 3 — WebRTC transfer and phone-file browser completion

**Files:**
- Modify `DeskLink/src/device_links/webrtc/{transfer_manager.rs,coordinator.rs}`.
- Create `DeskLink/src/device_links/webrtc/file_browser.rs`.
- Modify `DeskLink/src/device_links/daemon/file_transfer.rs`, `DeskLink/src/window.{rs,ui}`.
- Modify Android `WebRtcFileTransferManager.kt`, `WebRtcPhoneFileBrowser.kt`, and their tests.

- [ ] Write failing restart tests: persisted receive checkpoints resume at the verified offset after a new manager instance on Rust and Android.
- [ ] Implement timeout/backoff, pause/cancel/error, progress events, safe cleanup, and sender restoration.
- [ ] Implement `desklink.file.browser.v1` over file-control: SAF roots and opaque entry IDs only; list/navigate/metadata/upload/download/rename/move/create-folder/confirmed-delete.
- [ ] Add desktop browsable boxed-list UI with scrolling, breadcrumbs, transfer progress, and destructive confirmation.
- [ ] Test every chunk boundary interruption, wrong token/device/session/generation/checksum, traversal, symlink, collision, cancel, and atomic publication.
- [ ] Remove raw payload TLS and SFTP only after Rust↔Android fixtures pass in both directions. Commit `feat: move files and phone browsing to WebRTC`.

## Task 4 — Remote input with one GNOME portal lease

**Files:**
- Create `DeskLink/src/platform/remote_desktop_portal.rs` and `DeskLink/src/device_links/features/input.rs`.
- Modify desktop WebRTC peer, `remote_control.rs`, Android `WebRtcTransport.kt`, and `MouseReceiverService.java`.

**State:**

```rust
enum InputLeaseState { NotRequested, Requesting, Granted, Denied, Closed }
```

- [ ] Write failing adapter tests: packets before `Granted` are rejected; repeated packets while `Requesting` make one portal request; disconnect releases keys/buttons.
- [ ] Implement `CreateSession -> SelectDevices -> Start -> Request::Response`; retain one session per current device binding.
- [ ] Send keys/buttons via reliable channel and pointer/presenter motion via realtime channel.
- [ ] Require an explicit UI retry after denial/closure; never open permission UI from incoming packets.
- [ ] Reject Android control when accessibility service is disabled or the lease belongs to another session.
- [ ] Run portal fake and Android accessibility tests; commit `feat: add WebRTC input leases`.

## Task 5 — VP8 screen tracks in both directions

**Files:**
- Create `DeskLink/src/platform/screen_cast_portal.rs` and `DeskLink/src/device_links/features/screen.rs`.
- Modify desktop `webrtc/peer_connection.rs`, `remote_control.rs`, `window.{rs,ui}`.
- Modify Android `WebRtcTransport.kt`, `ScreenControlPlugin.kt`, `PhoneScreenCaptureService.kt`, and `ScreenProjectionActivity.kt`.

- [ ] Write failing Rust test: portal denial/closure clears the track and emits an unavailable state. Write Android test: MediaProjection denial never starts the capture service.
- [ ] Implement phone-to-desktop first: `ScreenCapturerAndroid` VP8 track -> desktop `webrtcbin` incoming pad -> `gtk4paintablesink` aspect-fit preview.
- [ ] Implement desktop-to-phone: ScreenCast portal -> PipeWire -> VP8 `webrtcbin` track -> Android `SurfaceViewRenderer` aspect-fit display.
- [ ] Use event-channel request/ready/stop/error controls; no JPEG payload frames or audio.
- [ ] Release tracks, renderers, portal sessions, PipeWire descriptors, projection, and foreground service on stop/revocation/disconnect/replacement.
- [ ] Test stream loss, cancellation, peer replacement, and bounded frame queue behaviour; commit `feat: add bidirectional WebRTC screen tracks`.

## Task 6 — Remaining backends and phone-authoritative feature models

**Files:**
- Create desktop adapters `platform/{upower,mpris,audio,logind}.rs`.
- Create desktop handlers for battery, MPRIS, volume, lock, notifications, SMS, contacts, telephony, connectivity, presenter, and commands.
- Modify Android feature/plugin tests and desktop UI state.

- [ ] Write failing availability tests: absent host backend omits its capability and exposes an unavailable state instead of fake empty success.
- [ ] Implement UPower monitoring; MPRIS discovery/control; PipeWire/PulseAudio sink state/control; logind lock state/control; fixed-argument command allowlists.
- [ ] Implement Android-authoritative SMS/contact/call/connectivity models. Enable call answer/reject/mute only with the necessary Android Telecom/InCall/default-dialer role; otherwise show unavailable.
- [ ] Complete notification resync, replacement, dismiss, reply, and action IDs.
- [ ] Add scrolling accessible GNOME action rows for notifications, errors, transfers, SMS/call state, and backend availability.
- [ ] Run feature tests and verify no unavailable handler is advertised. Commit `feat: complete WebRTC feature backends`.

## Task 7 — Recovery and external-connection configuration

**Files:**
- Create desktop `webrtc/{recovery.rs,cloud_signaling.rs,ice_config.rs}`.
- Modify desktop coordinator/peer/session modules and Android WebRTC coordinator, recovery policy, signaling, and settings.

- [ ] Write failing fake-peer test: disconnect triggers one ICE restart; old generation is rejected; failed restart creates one replacement after 1/2/4/8/16/30-second bounded backoff.
- [ ] Implement desktop network-change monitoring, ICE restart, re-authentication, replacement, and transfer checkpoint preservation.
- [ ] Add `wss://`-only signed desktop signaling client and validate destination, paired sender, timestamp, replay ID, signature, attempt, and generation.
- [ ] Validate user-entered STUN/TURN URIs and expiry-bearing TURN credentials on both platforms; ship no endpoint or credentials.
- [ ] Make Android foreground-service/Wi-Fi/wake-lock state explicit and scoped to active WebRTC sessions.
- [ ] Run fake signaling/recovery plus 100-cycle replacement tests. Commit `feat: recover WebRTC sessions across network changes`.

## Task 8 — Physical acceptance and permanent cutover

**Files:**
- Create `DeskLink/docs/testing/{WEBRTC_INTEROPERABILITY.md,WEBRTC_RELIABILITY.md,WEBRTC_ACCESSIBILITY.md}`.
- Modify package manifests/AppStream and Android dependency documentation as needed.

- [ ] Add matching Rust/Android fixtures for envelopes, handover, capabilities, file controls/chunks, replay, stale generation, invalid channels, and oversized messages.
- [ ] Run desktop: `cargo fmt --check`, `cargo check`, `cargo test -- --test-threads=1`, `cargo clippy -- -D warnings`, Meson build, desktop-file validation, and AppStream validation.
- [ ] Run Android: `./gradlew testDebugUnitTest --no-daemon`, `./gradlew lintDebug`, `./gradlew assembleDebug`, then install with `adb install -r` when a device is visible.
- [ ] On a real same-Wi-Fi phone, record: pairing, 100 reconnects, both app restarts, Wi-Fi loss/recovery, ordinary features, phone browsing, interrupted/resumed transfers, ten minutes input, and 30 minutes screen sharing in each direction.
- [ ] Run GNOME keyboard-only, high-contrast, 200% text, 800×600, and Orca checks.
- [ ] Only after every automated and physical gate passes: enable the permanent desktop responder and remove `send_file_legacy` plus every paired-feature LAN send. Commit `feat: complete WebRTC-only DeskLink transport`.

## Coverage check

- Connection, authorization, and handover: Tasks 1–2.
- Files and browsing: Task 3.
- Remote control and screen sharing: Tasks 4–5.
- Every remaining feature backend: Task 6.
- Network stability and external configuration: Task 7.
- No-claim-without-real-device validation and irreversible cutover: Task 8.
