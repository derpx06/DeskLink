# DeskLink

DeskLink is a Rust/GTK GNOME client for DeskLink Protocol v9. It pairs with the
DeskLink Android application over the local network. It is a production-focused
GNOME client, not a full replacement for `kdeconnect-kde`, and v9 does not
promise interoperability with KDE Connect peers.

## Transport

Discovery, identity, pairing, and signed WebRTC signaling use TLS with pinned
certificates on the DeskLink port range. Paired features prefer the
authenticated WebRTC data channels once the handover completes, and use the
pinned-TLS link until then, so features keep working on networks where ICE
cannot establish. File bytes are the exception: they only ever travel on the
bounded WebRTC file-data channel.

## Implemented features

Every entry below has a working handler in this application *and* in the
DeskLink Android application. The active set is defined in
`src/device_links/features/registry.rs`, and the matching Android set is
`WebRtcFeatureProfile`. The two are compared for exact equality during the
handover, so they must be edited together.

Received from the phone:

- Ping.
- Battery status.
- Clipboard content.
- Shared text and URLs.
- Notification mirroring, with in-memory notification state.
- SFTP browsing responses.
- Find-this-device requests.
- Lock and unlock requests for this session.

Sent to the phone:

- Ping and find-my-phone requests.
- Clipboard synchronization while paired.
- Text and URL sharing.
- Remote input through `desklink.mousepad.request`.
- Lock requests.
- Notification dismiss, reply, and action.
- SFTP browsing requests.
- File transfer, over the WebRTC file-data channel.

Plus the GNOME UI for discovery, paired-device actions, preferences, shortcuts,
and remote-control input.

## Not implemented

MPRIS media control, system volume control, and remote command execution are
present in this application's UI and packet layer but are **not** advertised as
capabilities. The DeskLink Android application implements those three plugins in
the opposite direction only — the phone controls the desktop — and has no
receiver for the requests this client would send. Advertising them would produce
nothing but rejected packets. They need an Android receiver plugin, or a desktop
MPRIS/PulseAudio backend to answer the phone's requests, before they can be
enabled.

Also outside this scope, as in the upstream KDE desktop client: SMS, contacts,
telephony, virtual monitor, presenter, digitizer, full mounted SFTP filesystem
integration, and full shared-input-device capture.

## Verification

Use:

```sh
cargo fmt --check
cargo check
cargo test
cargo clippy -- -D warnings
```

Packaging checks:

```sh
desktop-file-validate derx06.desklink.com.desktop
appstreamcli validate --no-net --explain data/derx06.desklink.com.metainfo.xml.in
```

The AppStream metadata intentionally keeps the existing application id
`derx06.desklink.com` and does not invent a homepage URL. Validators may report
those as packaging warnings until a final reverse-DNS id and project homepage
are chosen.
