# Contributing

Bug reports and pull requests are welcome. For anything large, open an issue
first so we can agree on the approach.

## Before sending a pull request

Desktop app:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Relay (in `relay/`):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Packaging metadata:

```sh
meson setup build && meson test -C build
```

Unit tests are inline `#[cfg(test)]` modules. Changes to pairing, handover,
reconnection or the UI also need a run on a real phone with the DeskLink
Android app; `cargo test` does not cover the two-device paths.

## Things that must not change

- The identifiers `derx06.desklink.com`, the `desklink` executable and the
  `~/.config/desklink/` directory. They are installed and persisted.
- The wire contract with the Android app. A change to packet types, envelopes,
  channels, the handshake or the enabled feature profile needs the matching
  change in the Android app. The WebRTC feature profile in
  `src/device_links/features/registry.rs` must equal Android's
  `WebRtcFeatureProfile` exactly.

## License

Contributions are accepted under GPL-3.0-or-later.
