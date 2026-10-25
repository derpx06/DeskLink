# Packaging DeskLink

| Directory | For |
|---|---|
| `aur/` | Arch Linux (AUR): `desklink` (source) and `desklink-bin` |
| `debian/` | Debian 13, Ubuntu 24.04 and newer (`.deb`) |
| `obs/` | Fedora 43+, openSUSE Tumbleweed / Leap 16 (`.rpm`), and the openSUSE Build Service |
| `../flatpak/` | Flatpak, any distribution |
| `../flake.nix` | Nix and NixOS |

All of them build with Meson, which runs `cargo build --locked`. The build
floor is Rust 1.85, GTK 4.12, GStreamer 1.22 and the libadwaita release named
by the `adw` feature in `Cargo.toml`. `Cargo.lock` is held to Rust 1.85
(Debian 13's compiler): `idna_adapter` stays at 1.1 and `image` at 0.25.6.
Check `rust-version` before running `cargo update`.

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
