# DeskLink

Use your Android phone and your GNOME desktop together. DeskLink is a Rust,
GTK 4 and libadwaita desktop app; it pairs with the DeskLink Android app.

- See your phone's screen on the desktop, with low latency.
- Control the computer's mouse and keyboard from the phone, after you approve
  it through the GNOME remote desktop prompt.
- Shared terminals between phone and desk, including one tap to start Claude
  Code or Codex on the computer.
- Phone notifications on the desktop, with replies and actions.
- Clipboard sync in both directions, and text and link sharing.
- File transfer both ways, and a browser for the phone's files.
- A presentation remote, ping, find-my-phone, battery status, and locking the
  desktop session from the phone.
- Away from home: the DeskLink relay connects your devices across networks.
- No account. Devices pair directly by scanning a code.

DeskLink speaks DeskLink Protocol v9. It grew out of KDE Connect but is not a
KDE product and does not interoperate with KDE Connect peers.

## Install

### Flathub

Once the app is published on Flathub:

```sh
flatpak install flathub derx06.desklink.com
```

The sandboxed build keeps its own pairing records, so phones paired with
another install of DeskLink need to pair again. See
[`flatpak/README.md`](flatpak/README.md) for its permissions and known
limitations.

### Arch Linux (AUR)

```sh
yay -S desklink       # builds the tagged release from source
yay -S desklink-bin   # installs the prebuilt release tarball
```

### GitHub Releases

Each [release](https://github.com/derpx06/DeskLink/releases) has
`desklink-<version>-x86_64.tar.gz` (the app, built for `/usr`), a Flatpak
bundle, and the relay server binary. The tarball is built on Arch Linux, so it
needs a current distribution (GStreamer 1.28, GTK 4.16, libadwaita 1.6, and a
recent glibc). Install it with:

```sh
tar -xzf desklink-1.0.0-x86_64.tar.gz
sudo cp -r desklink-1.0.0-x86_64/usr/. /usr/
sudo gtk-update-icon-cache -qtf /usr/share/icons/hicolor
```

Prefer a package from your distribution's tooling when one exists; files copied
this way are not tracked by the package manager.

### Runtime requirements

GStreamer 1.28 with webrtcbin (gst-plugins-bad), libnice, the PipeWire,
GTK 4 and libav (or another H.264 decoder) plugins, and the good plugins (VP8,
RTP). Screen sharing and remote control use the xdg-desktop-portal ScreenCast
and RemoteDesktop portals (`xdg-desktop-portal-gnome` on GNOME).

## Build from source

Minimum versions: Rust stable, GTK 4.16, libadwaita 1.6, GStreamer 1.28.
`build.rs` runs `glib-compile-resources`, so the GLib tools must be installed
even for a plain `cargo build`.

Arch Linux:

```sh
sudo pacman -S --needed base-devel rust meson gtk4 libadwaita openssl libnice \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
  gst-plugin-pipewire gst-plugin-gtk4 gst-libav desktop-file-utils appstream
```

Fedora (44 or newer):

```sh
sudo dnf install meson cargo rust gtk4-devel libadwaita-devel openssl-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel \
  gstreamer1-plugins-good gstreamer1-plugin-openh264 gstreamer1-plugin-gtk4 \
  pipewire-gstreamer libnice-gstreamer1 desktop-file-utils appstream
```

Ubuntu (26.04 or newer; use [rustup](https://rustup.rs) if the packaged Rust is
too old):

```sh
sudo apt install build-essential meson cargo rustc libgtk-4-dev libadwaita-1-dev \
  libssl-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-libav gstreamer1.0-pipewire gstreamer1.0-nice gstreamer1.0-gtk4 \
  desktop-file-utils appstream
```

With Meson (installs the desktop file, icons, AppStream metadata and D-Bus
service; builds an optimised binary by default):

```sh
meson setup build --prefix=/usr
meson compile -C build
meson test -C build
sudo meson install -C build
```

With Cargo only (just the binary; useful for development):

```sh
cargo build --release
./target/release/desklink
```

Release builds should set `DESKLINK_RELAY_URL=wss://<your relay>/v1` in the
environment at build time; it becomes the default relay address. Without it
the default is a placeholder, and users must enter a relay in Preferences.

## Away from home

On the same network, devices find each other and connect directly. When they
are apart, DeskLink can reach the phone through the **DeskLink relay**, a small
WebSocket server that only forwards connection setup. Turn it on under
Preferences → Away From Home, where you can also point DeskLink at your own
relay and choose whether media may fall back to a TURN server.

To run your own relay, see [`relay/deploy/DEPLOY.md`](relay/deploy/DEPLOY.md)
and [`relay/README.md`](relay/README.md).

## Privacy

- No account, no sign-in, no analytics.
- Pairing pins each device's certificate. Paired features travel only over
  authenticated, encrypted WebRTC connections between your devices.
- Through the relay, connection setup is end-to-end encrypted with keys
  derived from your pairing; the relay cannot read it. It does see which
  paired devices are online and talking to each other, their IP addresses
  (kept in memory only), and message sizes and timing. Screen video, files,
  clipboard and terminal data never pass through it.
- If no direct path exists and TURN is allowed, encrypted media may flow
  through a TURN server, which sees addresses and traffic volume but not
  content.

Details are in [`SECURITY.md`](SECURITY.md).

## What it does not do

MPRIS media control, system volume control and remote command execution exist
in this app's packet layer but are **not** advertised: the Android app only
implements them in the phone-controls-desktop direction. SMS, contacts,
telephony and a mounted phone filesystem are out of scope.

## Development

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Feature capabilities are defined in
`src/device_links/features/registry.rs`; the WebRTC feature profile there must
match the Android app's `WebRtcFeatureProfile` exactly.

## License

GPL-3.0-or-later. See [`COPYING`](COPYING).

## Screenshots

The AppStream metadata references `data/screenshots/devices.png`,
`screen.png` and `files.png` on the `master` branch. **These images still need
to be added** before publishing to Flathub.
