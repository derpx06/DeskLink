# DeskLink

DeskLink is a phone-to-Linux connectivity application built with Rust, GTK,
and libadwaita.

Current transport: DeskLink Protocol v9 over the local network.

The inactive compatibility module documents the legacy KDE Connect-compatible
protocol v8; it is not advertised or transmitted by the active path.

Current support: same-LAN discovery and direct encrypted communication.

Not yet implemented: Internet relay, WebRTC, Wi-Fi Direct, remote shell,
phone-to-desktop screen capture, and a shared virtual library.

## Implemented features

- LAN broadcast and mDNS discovery with TLS device links on the DeskLink port
  range.
- Pair, accept, reject, and unpair device flows.
- Ping.
- File send and receive through compatible payload transfer packets.
- Text and URL sharing.
- Clipboard synchronization while paired.
- Remote input receive and send through `desklink.mousepad.request`.
- Lock request and lock status replies.
- Find phone requests.
- Battery status display.
- Phone notification mirroring with in-memory notification state.
- MPRIS media command packets, player/status refresh, and richer incoming
  status display.
- System volume status and request packets.
- Remote command list and execution request packets for commands advertised by
  the paired device.
- GVfs-backed SFTP mounting and browsing.
- GNOME UI for discovery, paired-device actions, preferences, shortcuts, and
  remote-control input.

## Not implemented

The upstream KDE desktop client has many plugins that are intentionally outside
this scope, including SMS, contacts, telephony, virtual monitor, presenter,
digitizer, and full shared-input-device capture.

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
