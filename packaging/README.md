# Packaging DeskLink

| Directory | For |
|---|---|
| `aur/` | Arch Linux (AUR): `desklink` (source) and `desklink-bin` |
| `debian/` | Debian 13, Ubuntu 24.04 and newer (`.deb`) |
| `obs/` | Fedora 43+, openSUSE Tumbleweed (`.rpm`; Leap 16 is untested), and the openSUSE Build Service |
| `../flatpak/` | Flatpak, any distribution |
| `../flake.nix` | Nix and NixOS |

All of them build with Meson, which runs `cargo build --locked`. The build
floor is Rust 1.85, GTK 4.12, GStreamer 1.22, VTE 0.76 (its GTK 4 build,
`vte-2.91-gtk4`) and the libadwaita release named by the `adw` feature in
`Cargo.toml`. `Cargo.lock` is held to Rust 1.85
(Debian 13's compiler): `idna_adapter` stays at 1.1 and `image` at 0.25.6.
Check `rust-version` before running `cargo update`.

## Supported distributions

Exact minimum versions (`src/meson.build` checks them before cargo runs):
GTK 4.12, libadwaita 1.5, GStreamer 1.22 (with `gstreamer-webrtc-1.0`),
OpenSSL 3.0, Rust 1.85, Meson 1.0. VTE 0.76 (`vte-2.91-gtk4`) is optional:
`-Dvte=auto` (the default) builds without it when it is missing, and the
Terminals page then shows a read-only preview with "Open in terminal
window", which runs the session in the system terminal.

Built and tested in clean chroots of each release (2026-11, `cargo test`
under Xvfb, then the package installed into a pristine root of the same
release and `desklink --check` run):

DISTRO_TABLE

Not supported natively: **Debian 12** (GTK 4.8, libadwaita 1.2, Rust 1.63,
no newer GTK in bookworm-backports) and Ubuntu 22.04 (GTK 4.6). DeskLink
uses about 130 widgets and calls from libadwaita 1.3 to 1.5 and GTK 4.10 to
4.12 (dialogs, navigation views, breakpoints, `FileDialog`/`FileLauncher`), so
there is no build for them: use the **Flatpak**, which carries its own GTK.
Derivatives follow their base: Linux Mint 22, Pop!_OS 24.04, Zorin OS 18 and
elementary OS 8 are Ubuntu 24.04; Manjaro and EndeavourOS are Arch.

Build dependencies, per family:

| Family | Packages |
|---|---|
| Debian 13, Ubuntu 24.04+ | `debhelper meson cargo rustc pkgconf gettext libglib2.0-dev-bin libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev libssl-dev desktop-file-utils` (Ubuntu 24.04: `rustc-1.85 cargo-1.85`) |
| Fedora | `cargo rust gcc meson gettext desktop-file-utils glib2-devel gtk4-devel libadwaita-devel vte291-gtk4-devel gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel openssl-devel` |
| openSUSE | `cargo rust gcc meson gettext-tools desktop-file-utils glib2-devel gtk4-devel libadwaita-devel vte-devel gstreamer-devel gstreamer-plugins-base-devel gstreamer-plugins-bad-devel libopenssl-devel` |
| Arch | `rust meson gtk4 libadwaita vte4 gstreamer gst-plugins-base gst-plugins-bad libnice openssl` |

Runtime: the packages above pull in what a fresh install needs to connect
(`webrtcbin`, `nicesrc`, DTLS/SRTP/SCTP, VP8, Opus, `pipewiresrc`/`pipewiresink`,
`ximagesrc` for X11 sessions without a portal); `desklink --check` lists
anything still missing with the install command for the distribution.

`gtk4paintablesink` is compiled into DeskLink and registered only when the
system has none, so no package needs the distribution's GTK 4 GStreamer
plugin (Ubuntu 24.04 has none).

## Offline builds: vendored crates

Package builders have no network. Ship the crates beside the source:

```sh
cargo vendor --locked vendor      # from the repository root
tar -czf vendor.tar.gz vendor
```

Both recipes use `vendor/` when it is present: the Debian rules write a cargo
config into `debian/cargo-home`, the spec writes `.cargo/config.toml` in
`%prep`. Without `vendor/`, the Debian build downloads crates (fine for a local
`dpkg-buildpackage`, not for sbuild or OBS).

## Debian and Ubuntu

```sh
sudo apt install devscripts equivs
cp -r packaging/debian debian         # dpkg-buildpackage wants ./debian
sudo mk-build-deps -ir debian/control # installs the Build-Depends
dpkg-buildpackage -us -uc -b
sudo apt install ../desklink_1.3.0_*.deb
```

The package is `3.0 (native)`: bump `debian/changelog` (`dch -v <version>`)
for every release. Ubuntu 24.04's default `rustc` is 1.75; the build
dependency falls back to the versioned `rustc-1.85`/`cargo-1.85` packages and
`debian/rules` puts `/usr/lib/rust-1.85/bin` first on `PATH` when it exists.
If those packages are not available, install Rust 1.85+ with rustup and build
with `dpkg-buildpackage -us -uc -b -d`.

Runtime dependencies are declared in `debian/control`: the base, good and bad
GStreamer plugins, `gstreamer1.0-nice`, `gstreamer1.0-pipewire` and
`xdg-desktop-portal`, with `gstreamer1.0-libav` (H.264) recommended.

## Fedora and openSUSE (rpmbuild)

```sh
mkdir -p ~/rpmbuild/SOURCES
git archive --prefix=desklink-1.3.0/ -o ~/rpmbuild/SOURCES/desklink-1.3.0.tar.gz HEAD
cargo vendor --locked vendor && tar -czf ~/rpmbuild/SOURCES/vendor.tar.gz vendor
sudo dnf builddep packaging/obs/desklink.spec   # openSUSE: zypper source-install -d, or install the BuildRequires
rpmbuild -ba packaging/obs/desklink.spec
```

`rpmspec --parse packaging/obs/desklink.spec` checks the spec without
building. Fedora's H.264 decoder lives outside the main repositories'
defaults (`gstreamer1-plugin-openh264` from the Cisco repository, or
`gstreamer1-plugin-libav`); without one DeskLink negotiates VP8.

## openSUSE Build Service

OBS builds the one source for Fedora, openSUSE, Debian and Ubuntu and
publishes a repository for each.

1. Create a project, e.g. `home:<you>:desklink`, and add the targets under
   *Repositories*: `Fedora_43`, `openSUSE_Tumbleweed`, `openSUSE_Leap_16.0`,
   `Debian_13`, `xUbuntu_24.04`, `xUbuntu_26.04`.
2. Create a package `desklink` and check it out:
   `osc checkout home:<you>:desklink/desklink`.
3. Copy in `packaging/obs/desklink.spec` and `packaging/obs/_service`, set the
   tag in `_service` (`revision`), and run `osc service manualrun`. It checks
   out the tag (`obs_scm`), vendors the crates **without** `cargo update`
   (`cargo_vendor`, `update=false`, so the Rust 1.85 pins hold), and sets the
   version. Commit the results: `osc addremove && osc commit`.
4. For the Debian/Ubuntu targets, also commit a `debian.tar.gz` of
   `packaging/debian` (`tar -czf debian.tar.gz -C packaging debian`) and a
   `desklink.dsc` listing the source tarballs; OBS builds `.deb`s from those.

Users then add the repository OBS shows on the package page (it gives
per-distribution `zypper ar` / `dnf config-manager` / `apt` instructions).

## Nix

```sh
nix build        # ./result/bin/desklink
nix develop      # cargo, clippy and every library, with GStreamer plugin paths set
```

The flake builds with Meson through `rustPlatform.cargoSetupHook` and
`importCargoLock`, so `Cargo.lock` is the only pin. `wrapGAppsHook4` puts the
GStreamer plugins on `GST_PLUGIN_SYSTEM_PATH_1_0` in the wrapper.
