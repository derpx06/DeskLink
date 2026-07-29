# WebRTC remote control and screen sharing

DeskLink carries remote-view video, input, clipboard, files, and notifications
over the authenticated WebRTC peer. LAN is limited to discovery, pairing,
identity, and signed SDP/ICE signaling; it is never a paired-feature fallback.

## Required host services

- GNOME Wayland: `xdg-desktop-portal`, a working GNOME portal backend,
  PipeWire, GStreamer WebRTC/VP8 plugins, and an active user D-Bus session.
- Android: DeskLink notification/background permissions as needed; screen
  sharing requires a fresh MediaProjection approval each time Android revokes
  it; controlling the phone requires the enabled DeskLink Accessibility
  service. Text input additionally requires DeskLink's input method.

The current Rust portal binding exposes the PipeWire node ID rather than
`pipewire-serial`. DeskLink treats that node ID as an in-memory value scoped
strictly to the live portal session: it is never persisted, reused after a
portal closure, or accepted from a peer. Upgrading to a binding that exposes
`pipewire-serial` remains a packaging/runtime follow-up.

## Manual validation checklist

Do not mark a release ready until all of the following have been performed on
a paired phone and desktop over the same Wi-Fi network:

1. Verify the transport diagnostic says WebRTC after mutual feature readiness.
2. Open the desktop remote view, approve phone capture, and confirm the full
   phone display is aspect-fit with no stale frame after stop.
3. Enable control explicitly; tap, drag, scroll, type, and use Back/Home/
   Recents. Reject input after release, lease expiry, or peer replacement.
4. From Android, request desktop viewing, approve the GNOME portal once, then
   enable control and confirm no repeated portal dialogs during interaction.
5. Lock then unlock Android. The WebRTC peer stays paired; phone capture moves
   to a visible paused state and requires explicit renewed MediaProjection
   approval.
6. Revoke the GNOME portal. Input stops immediately, pressed controls release,
   capture clears, and Android shows a recoverable permission state.
7. Disconnect and reconnect Wi-Fi, then repeat both directions. Confirm stale
   sessions cannot inject input after the new WebRTC generation is ready.

Physical-device and portal validation is intentionally not implied by the
unit tests; platform consent and capture behavior must be checked manually.
