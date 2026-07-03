# DeskLink

DeskLink is a Rust/GTK GNOME client for the KDE Connect LAN protocol. It is a
production-focused GNOME client, not a full replacement for `kdeconnect-kde`.

## Implemented features

- LAN discovery and TLS device links on the KDE Connect port range.
- Pair, accept, reject, and unpair device flows.
- Ping.
- File send and receive through KDE Connect payload transfer packets.
- Text and URL sharing.
- Clipboard synchronization while paired.
- Remote input receive and send through `kdeconnect.mousepad.request`.
- Lock request and lock status replies.
- Find phone requests.
- Battery status display.
- Phone notification mirroring with in-memory notification state.
- MPRIS media command packets, player/status refresh, and richer incoming
  status display.
- System volume status and request packets.
- Remote command list and execution request packets for commands advertised by
  the paired device.
- SFTP browsing request and response display.
- GNOME UI for discovery, paired-device actions, preferences, shortcuts, and
  remote-control input.

## Not implemented

The upstream KDE desktop client has many plugins that are intentionally outside
this scope, including SMS, contacts, telephony, virtual monitor, presenter,
digitizer, full mounted SFTP filesystem integration, and full shared-input-device
capture.

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
