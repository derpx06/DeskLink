# DeskLink

Use your Android phone and your Linux desktop together (checked on GNOME, KDE
Plasma, Xfce, Cinnamon, MATE and sway). DeskLink is a Rust, GTK 4 and libadwaita
desktop app; it pairs with the DeskLink Android app (Android 7.0 or newer).

**0.3.0 is a beta**, the first public release: it works, and it is still being
shaped. Please report problems on the issue tracker.

- See your phone's screen on the desktop, with low latency.
- Control the computer's mouse and keyboard from the phone, after you approve
  it through the GNOME remote desktop prompt.
- Shared terminals between phone and desk, including one tap to start Claude
  Code or Codex on the computer.
- Phone notifications on the desktop, with replies and actions.
- Clipboard sync in both directions, and text and link sharing.
- File transfer both ways, and a Bucket folder kept in step.
- Ping, find-my-phone, battery status, lock and unlock from the phone, notes
  and snippets, contacts and calls on the computer, media control both ways,
  the phone's microphone or sound here and this computer's sound there, and
  Glide (the pointer carrying on onto the phone).
- Works on your Wi-Fi network (LAN). Connecting away from home through the
  DeskLink relay is built but switched off in 0.3 (see "Away from home").
- No account. Devices pair directly by scanning a code.

DeskLink speaks DeskLink Protocol v9. It grew out of KDE Connect but is not a
KDE product and does not interoperate with KDE Connect peers.

## Install

### Supported distributions

| Distribution | How | Notes |
|---|---|---|
| Any (x86_64) | Flatpak bundle from GitHub Releases | Everything bundled; not a sandbox (see below) |
| Debian 13 (trixie) | native `.deb` ([`packaging/debian`](packaging/debian)) | Built with Debian's own Rust 1.85 |
| Ubuntu 24.04 LTS and newer | native `.deb` | 24.04 needs the `rustc-1.85` toolchain package; it ships no `gstreamer1.0-gtk4`, which DeskLink bundles |
| Fedora 43 and newer | native RPM ([`packaging/obs`](packaging/obs)) | H.264 needs `gstreamer1-plugin-openh264` or `gstreamer1-plugin-libav` |
| openSUSE Tumbleweed (Leap 16 untested) | native RPM (OBS) | H.264 needs `gstreamer-plugins-libav` (Packman) |
| Arch Linux, Manjaro | AUR | |
| NixOS and Nix | `nix run github:derpx06/DeskLink` ([`flake.nix`](flake.nix)) | |

Without an H.264 decoder DeskLink still works: it asks the phone for VP8
instead, which costs the phone more power.

Controlling this computer from the phone depends on the desktop:

| Desktop | Phone sees the screen | Phone controls it |
|---|---|---|
| GNOME, KDE Plasma | yes (portal) | yes (portal) |
| Cinnamon, MATE, XFCE and other X11 desktops | yes (X11 fallback, native packages only: the Flatpak runtime has no X11 capture) | yes (XTest), after approving the phone at the desk under Remote Control |
| COSMIC, wlroots compositors (Sway, Hyprland, ...) | yes where the portal offers ScreenCast, view only (sway checked; COSMIC expected) | no: there is no RemoteDesktop portal |

Settings, Advanced, System check (or `desklink --check`) lists what works on
your computer and the packages to install for the rest.

The X11 fallback is used only when the session is X11 and the portal offers no
screen sharing. It captures every monitor as one picture.

### Flatpak bundle

```sh
flatpak install --user ./desklink-0.3.0-x86_64.flatpak
```

The bundle is on GitHub Releases. It is not on Flathub: shared terminals need
`--talk-name=org.freedesktop.Flatpak`, which removes the sandbox (see
[`SECURITY.md`](SECURITY.md)).

The Flatpak build keeps its own pairing records, so phones paired with
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
needs a glibc at least as new as Arch's; elsewhere use the Flatpak or a native
package. Install it with:

```sh
tar -xzf desklink-0.3.0-x86_64.tar.gz
sudo cp -r desklink-0.3.0-x86_64/usr/. /usr/
sudo gtk-update-icon-cache -qtf /usr/share/icons/hicolor
```

Prefer a package from your distribution's tooling when one exists; files copied
this way are not tracked by the package manager.

### Debian, Ubuntu, Fedora, openSUSE

Build a package from this repository with the recipes in
[`packaging/`](packaging/README.md) (`dpkg-buildpackage`, `rpmbuild`, or the
openSUSE Build Service, which publishes repositories for all of them).

### Nix

```sh
nix run github:derpx06/DeskLink
nix develop    # a shell with every build dependency
```

### Runtime requirements

GStreamer 1.22 or newer with webrtcbin (gst-plugins-bad), libnice, the
PipeWire plugin, and the good plugins (VP8, RTP, ximagesrc); optionally libav
or another H.264 decoder. `gtk4paintablesink` is built into DeskLink, so the
distribution's GTK 4 GStreamer plugin is optional. Screen sharing and remote
control use the xdg-desktop-portal ScreenCast and RemoteDesktop portals
(`xdg-desktop-portal-gnome` on GNOME, `-kde` on Plasma), or the X11 fallback
above.

## Build from source

Minimum versions: Rust 1.85, GTK 4.12, GStreamer 1.22, VTE 0.76 (GTK 4
build), and the libadwaita release named by the `adw` feature in
`Cargo.toml`. Without VTE, `cargo build --no-default-features` still builds;
the Terminals page then shows a read-only preview instead of a terminal.
`build.rs` runs `glib-compile-resources`, so the GLib tools must be installed
even for a plain `cargo build`.

Arch Linux:

```sh
sudo pacman -S --needed base-devel rust meson gtk4 libadwaita vte4 openssl libnice \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
  gst-plugin-pipewire gst-libav desktop-file-utils appstream
```

Fedora (43 or newer):

```sh
sudo dnf install meson cargo rust gtk4-devel libadwaita-devel vte291-gtk4-devel openssl-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel \
  gstreamer1-plugins-good gstreamer1-plugin-openh264 \
  pipewire-gstreamer libnice-gstreamer1 desktop-file-utils appstream
```

Debian 13 and Ubuntu 24.04 or newer (on 24.04 use [rustup](https://rustup.rs)
or the `rustc-1.85` package, the default Rust is too old):

```sh
sudo apt install build-essential meson cargo rustc libgtk-4-dev libadwaita-1-dev \
  libvte-2.91-gtk4-dev libssl-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-libav gstreamer1.0-pipewire gstreamer1.0-nice \
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
there is no default relay, and one can be set as `relay_url` in
`protocol/config.json`.

## Away from home

**Off in 0.3.** This release is LAN-only. The relay is controlled only in
code, by `RELAY_ENABLED` in `src/device_links/relay_link/mod.rs` (and
`BuildConfig.RELAY_ENABLED` in the Android app); there is no setting to turn
it or TURN on or off. The rest of this section describes the feature for a
build with it switched on.

On the same network, devices find each other and connect directly. When they
are apart, DeskLink can reach the phone through the **DeskLink relay**, a small
WebSocket server that only forwards connection setup; media that cannot go
direct falls back to the relay's TURN server. To use your own relay, set
`relay_url` (`wss://host/v1`) in `~/.config/desklink/protocol/config.json`
(Flatpak: `~/.var/app/derx06.desklink.com/config/desklink/protocol/config.json`);
there is no UI for it.

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
- If no direct path exists, encrypted media may flow
  through a TURN server, which sees addresses and traffic volume but not
  content.

Details are in [`SECURITY.md`](SECURITY.md).

## Updates

DeskLink tells you, when you open it, that a newer version exists and shows the right way to update for
how you installed it (it never installs anything itself). The check is one anonymous HTTPS request a day
for a signed file; no name, device ID or pairing data is sent. Turn it off in Settings > About. Details:
[`docs/UPDATES.md`](docs/UPDATES.md).

## What it does not do

Browsing the phone's files, the presentation remote, system volume, run
commands, the drawing pad, the webcam and Bluetooth were removed in 0.3. SMS and
a mounted phone filesystem are out of scope.

## Development

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Feature capabilities are defined in
`src/device_links/features/registry.rs`; the WebRTC feature profile there must
match the Android app's `WebRtcFeatureProfile` exactly.

## License

GPL-3.0-or-later. See [`COPYING`](COPYING).

## Uninstalling

Removing the package or the Flatpak leaves what the app wrote under your home
folder: `~/.config/desklink/` (pairings and settings, on purpose; delete it to
forget every phone), the top-bar extension copy in
`~/.local/share/gnome-shell/extensions/desklink@derx06.github.io` (turn it off
with `gnome-extensions disable desklink@derx06.github.io` first), and the
search provider file `~/.local/share/gnome-shell/search-providers/derx06.desklink.com.search-provider.ini`.
Flatpak: `flatpak uninstall --delete-data derx06.desklink.com`.
