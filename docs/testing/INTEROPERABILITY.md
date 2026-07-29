# DeskLink interoperability checklist

This checklist covers the cross-device paths changed in the reliability fix. It
must be run with one Linux desktop and one Android device on the same LAN.

## Automated checks

- [x] Rust formatting, tests, and clippy pass.
- [x] Android debug unit tests, lint, and debug APK build pass.
- [x] Desktop launcher validation passes on generated metadata.
- [x] GUI D-Bus object introspection and `ListDevices` work in a live session.
- [ ] Install the generated APK on a connected physical device.

## Physical-device gate

- [ ] Discover and pair the phone without changing the existing device ID.
- [ ] Send a small file in both directions and verify its contents.
- [ ] Cancel a large transfer; confirm only a `.desklink-*.part` temporary file remains.
- [ ] Restart Android and desktop; verify pairing and transfer recovery.
- [ ] Disconnect and reconnect Wi-Fi; verify the session reconnects once.
- [ ] Reconnect the Android notification listener; verify one invalid icon does not stop resync.
- [ ] Approve RemoteDesktop permission once; run ten minutes of continuous input without
      another prompt.
- [ ] Deny, cancel, and close the portal session; verify the UI reports the state and
      offers an explicit retry.
- [ ] Open desktop-to-phone screen sharing and verify the complete phone view.
- [ ] Open phone-to-desktop screen sharing and verify JPEG frames, aspect fit, and stop
      cleanup.
- [ ] Repeat forced disconnect/reconnect 100 times; verify no duplicate sessions, lost
      pairing records, corrupt files, or uncaught exceptions.

The physical-device items are intentionally not marked complete by build-only
validation.
