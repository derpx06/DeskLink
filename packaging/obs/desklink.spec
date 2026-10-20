#
# spec file for package desklink
#
# Builds on Fedora 43+ and openSUSE (Tumbleweed, Leap 16), locally with
# rpmbuild or on the openSUSE Build Service. Sources: see packaging/README.md.
#

%global appid derx06.desklink.com
# Rust release builds carry no debug info, which leaves find-debuginfo with
# empty file lists on Fedora.
%global debug_package %{nil}

Name:           desklink
Version:        1.0.0
Release:        0%{?dist}
Summary:        Use an Android phone and a Linux desktop together
License:        GPL-3.0-or-later
URL:            https://github.com/derpx06/DeskLink
Source0:        %{name}-%{version}.tar.gz
# `cargo vendor` of Cargo.lock: the build runs offline.
Source1:        vendor.tar.gz

BuildRequires:  cargo >= 1.85
BuildRequires:  rust >= 1.85
BuildRequires:  gcc
BuildRequires:  meson >= 1.0.0
BuildRequires:  gettext
BuildRequires:  desktop-file-utils
BuildRequires:  /usr/bin/glib-compile-resources
BuildRequires:  /usr/bin/glib-compile-schemas
BuildRequires:  pkgconfig(gtk4) >= 4.12
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(gstreamer-1.0) >= 1.22
BuildRequires:  pkgconfig(gstreamer-base-1.0)
BuildRequires:  pkgconfig(gstreamer-video-1.0)
BuildRequires:  pkgconfig(gstreamer-sdp-1.0)
BuildRequires:  pkgconfig(gstreamer-webrtc-1.0)
BuildRequires:  pkgconfig(openssl)

Requires:       xdg-desktop-portal
%if 0%{?suse_version}
Requires:       gstreamer-plugins-base
Requires:       gstreamer-plugins-good
Requires:       gstreamer-plugins-bad
Requires:       gstreamer-plugins-libnice
Requires:       gstreamer-plugin-pipewire
# H.264 for phones that encode their screen in hardware; VP8 otherwise.
Recommends:     gstreamer-plugins-libav
%else
Requires:       gstreamer1-plugins-base
Requires:       gstreamer1-plugins-good
Requires:       gstreamer1-plugins-bad-free
Requires:       libnice-gstreamer1
Requires:       pipewire-gstreamer
# H.264 for phones that encode their screen in hardware; VP8 otherwise.
Recommends:     (gstreamer1-plugin-libav or gstreamer1-plugin-openh264)
%endif

%description
DeskLink pairs a Linux desktop with the DeskLink Android app over the local
network: notifications, clipboard, file transfer, shared terminals, and
viewing or controlling either screen from the other device.

%prep
%autosetup -p1 -a1
# Found by cargo from meson's build directory inside the tree, for the build
# and for `meson install`, which runs cargo again.
mkdir -p .cargo
cat > .cargo/config.toml <<'CARGOEOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"

[net]
offline = true
CARGOEOF

%build
%meson
%meson_build

%install
%meson_install

%check
%meson_test

%files
%license COPYING
%doc README.md
%{_bindir}/%{name}
%{_datadir}/%{name}/
%{_datadir}/applications/%{appid}.desktop
%{_datadir}/metainfo/%{appid}.metainfo.xml
%{_datadir}/glib-2.0/schemas/%{appid}.gschema.xml
%{_datadir}/dbus-1/services/%{appid}.service
%{_datadir}/icons/hicolor/scalable/apps/%{appid}.svg
%{_datadir}/icons/hicolor/symbolic/apps/%{appid}-symbolic.svg

%changelog
