# DeskLink 1.3 on Linux: distributions and desktops

What builds and runs where, and what each feature does on each desktop. Rows
that cannot work say so in **Settings → Advanced → System check** and in
`desklink --check`, with the one step to fix them (`src/platform.rs`). Longer
background: `phone_connect/docs/LINUX-SUPPORT.md` and `DESKTOP-INTEGRATION.md`.

Checked 2026-11-15. **R** = run (in a rootless chroot of the distribution, or a
nested session on the author's machine), **T** = unit test with a faked desktop,
**C** = code reading only.

## Distributions

Each one: its own packages from `packaging/` installed, `cargo build --release
--locked` with the distribution's own Rust, then the binary started under
Xvfb on a private D-Bus with no desktop (window up, daemon running, `--check`).

| Distribution | GTK / adw / GStreamer / VTE / glib | Rust | Release build | Launch (Xvfb) + `--check` | Tests |
|---|---|---|---|---|---|
| Ubuntu 24.04 LTS | 4.14 / 1.5 / 1.24 / 0.76 / 2.80 | 1.85 (`rustc-1.85`) | yes, 6 min | yes R | UBUNTU_TESTS |
| Debian 13 | 4.18 / 1.7 / 1.26 / 0.80 / 2.84 | 1.85 | yes, 7 min | yes R | not run |
| Fedora 45 | 4.24 / 1.10 / 1.28 / 0.84 / 2.90 | 1.99 | yes, 4 min | yes R | not run |
| openSUSE Tumbleweed (20261111) | 4.24 / 1.10 / 1.28 / 0.84 / 2.90 | 1.99 | yes, 5 min | yes R | not run |
| Arch (2026-11) | 4.24 / 1.10 / 1.28 / 0.84 / 2.90 | 1.100 | yes, 5 min | yes R, plus sway and Xfce below | host gate (Manjaro) |

Each launch: window mapped (`xwininfo`), alive after 25 s with no panic,
`--check` exit 0, `--version`/`--help` answer without a display (fixed in this
pass: they used to print the no-display error). No new API floor was found:
the build is clean from libadwaita 1.5 / GTK 4.14 up to 1.10 / 4.24.

Floor (checked by `src/meson.build`): GTK 4.12, libadwaita 1.5, GStreamer 1.22,
OpenSSL 3.0, Rust 1.85; VTE 0.76 optional. Ubuntu 24.04 is the oldest native
target; Debian 12 and Ubuntu 22.04 are below it (use the Flatpak).

## Desktops

`yes` works · `diff` works, differently (the cell says how) · `no` not possible
there; the System check and the feature's own row say why and give the step.

| Feature | GNOME (W/X11) | Plasma 6 Wayland | Plasma X11 | Xfce, Cinnamon, MATE, Budgie (X11) | wlroots: sway, Hyprland, labwc, river, niri | COSMIC | Plain X11 WM, no portal |
|---|---|---|---|---|---|---|---|
| Pair, LAN link, files, phone notifications, terminals | yes | yes R | yes | yes R(Xfce) | yes R(sway) | yes | yes R |
| Tray / panel | own top-bar extension (or AppIndicator) | System Tray | System Tray | panel's StatusNotifier applet (the row names it) | Waybar/swaybar tray | Notifications Tray | needs a tray host (snixembed); else the window reopens from a notification |
| Clipboard both ways | yes (via Xwayland) | yes | yes | yes | yes with Xwayland; without it only phone → desk, said (1) | as wlroots | yes |
| Send to phone from the file manager | Files: Scripts | Dolphin menu | Dolphin menu | Thunar "Send To", Nemo and Caja Scripts (2) | "Open With" entry | "Open With" entry | "Open With" entry |
| Search from the desktop | Activities search | KRunner | KRunner | in-app palette | in-app palette | in-app palette | in-app palette |
| This screen on the phone, phone controls it | yes (portal) | yes (portal) R(check) | yes, X11 capture + XTest after approval at the desk | same as Plasma X11 R(check) | diff: view only (3) | diff: view only unless its portal has RemoteDesktop | yes, X11 capture R(check) |
| Phone screen on this computer | yes | yes | yes | yes | yes | yes | yes |
| Phone as a second display (Extend) | Wayland only | yes, no per-monitor scale | no | no | no | no | no |
| Glide (pointer onto the phone) | GNOME 45+, Wayland | Plasma 6.1+ | no | no | no | no | no |
| Sound with the phone | PipeWire (PulseAudio-only: no, install line given) | same | same | same | same | same | same |
| Lock from the phone | yes | yes | yes | yes (their screensavers) | needs swayidle/hypridle | yes | needs xss-lock or a locker |
| Unlock with the phone | yes | yes | yes | limited: not promised | swaylock/hyprlock yes; others refused, said | limited | i3lock & co. refused, said |
| Start at login | yes | yes | yes | yes | no: the `exec` line to add is shown | yes | no: the line is shown |
| Keep awake while linked | yes (logind/elogind) | yes | yes | yes | yes | yes | yes |
| Do Not Disturb shared | yes | no, said (4) | no, said | no, said | no, said | no, said | no, said |

1. New in this pass: a "Shared clipboard" row (Limited) when a Wayland session
   has no `DISPLAY`; DeskLink reads the clipboard through X11 because Wayland
   lets only the focused window read it. T
2. New: Nemo (`~/.local/share/nemo/scripts`) and Caja (`~/.config/caja/scripts`)
   get the same per-phone scripts as Nautilus; Thunar gets one entry per phone in
   `~/.local/share/Thunar/sendto` (its Send To menu). T
3. New: with ScreenCast but no RemoteDesktop (xdg-desktop-portal-wlr,
   -hyprland) the share opens a ScreenCast-only session and the phone gets the
   view-only state it already had for a GNOME grant without remote interaction.
   The row is Limited ("can watch this screen but not control it"), the
   approve-control button is off with that reason, Extend gives its own reason.
   Before: no share at all. T + R(see below)
4. Fixed: DND was mirrored into GNOME's `show-banners` on any desktop that had
   the schema installed (Plasma, Xfce, Cinnamon usually do), where nothing reads
   it. It is now GNOME Shell only, and the row says so elsewhere. C

## Next to KDE Connect and GSConnect

Plasma ships `kdeconnectd`; GSConnect's daemon can run on GNOME even with its
extension off (its Files helper starts it over D-Bus). Both use UDP 1716 with
`SO_REUSEADDR`, as DeskLink does, and TCP from 1716 up.

- **TCP:** DeskLink takes the first free port in 1716-1764 and announces it in
  its identity; no clash. C
- **Broadcast beacons** (phone → `255.255.255.255:1716`) reach every socket. R
- **Unicast beacons** (the phone's announcement to the address it last reached
  the desk on, used when a paired desk has no link) reach only **one** socket
  among IPv4 listeners on the same port, the last bound. Measured (and pinned
  by `a_unicast_datagram_reaches_one_of_two_sockets_sharing_a_port`): two IPv4
  `SO_REUSEADDR` sockets get one copy between them; a dual-stack IPv6 socket
  does **not** take IPv4 unicast from DeskLink's IPv4 socket, in either bind
  order. GSConnect on this laptop holds `[::]:1716` (dual-stack) and KDE
  Connect binds dual-stack too, so they do not take DeskLink's IPv4 beacons;
  they do get IPv6 ones, which DeskLink never listened for. A slow reconnect
  here is therefore not explained by GSConnect stealing IPv4 beacons. R
- New: the probe reads `/proc/net/udp` and names any other program with an
  IPv4 socket on 1716 (KDE Connect, GSConnect, Valent, Zorin Connect, or its
  command name); the "Finding your phone" row turns Limited and stays as a
  banner until dismissed: "<app> also listens for phones on this computer and
  can take the messages your phone sends DeskLink... Turn <app> off, then
  restart DeskLink." Probed at start, 5 s later and when the System check
  opens. T
- Not done (needs the phone): a port DeskLink owns alone. The desk would bind
  an exclusive UDP port (no `SO_REUSEADDR`) and announce it as `udpPort` in
  its identity; the phone would send its unicast beacons there (broadcasts stay
  on 1716 for first discovery, `_desklink._udp` mDNS unchanged), plus an IPv6
  listener so IPv6 unicast beacons are heard at all.

## Nested and headless sessions run for this table

All in their own network namespace (loopback only), private D-Bus, HOME and
runtime dir; never the live session. `desklink --check` in each, window
screenshotted.

| Session | Result |
|---|---|
| Plasma 6.7 Wayland: nested `kwin_wayland` (X11 backend inside Xvfb) + `plasmashell`, xdg-desktop-portal-kde | Window up; tray icon in the System Tray (tray row ok); share, Extend, Glide, unlock rows ok through the KDE portal; sound missing (no PipeWire in the sandbox, install line given). Without Xwayland the new clipboard row says so. |
| sway (headless, pixman) + xdg-desktop-portal-wlr + PipeWire, Arch chroot | Window up; share row **limited, view only**; Extend and Glide missing with reasons; start-at-login gives the `exec` line; tray row names Waybar/swaybar. The ignored test `a_real_portal_share_is_view_only_without_remote_desktop` opened a real ScreenCast-only session through xdpw: `remote desktop: false; stream 1280x860 node 46; devices 0`. |
| Xfce (xfwm4, xfce4-panel, xfce4-notifyd, Xvfb), Arch chroot | Window up; DeskLink's icon in the panel's Status Tray (tray ok); notifications ok; share ok through X11 capture; Extend/Glide missing with reasons. |
| Plain X11, no desktop name, portal without backends, no tray, no notification daemon (each distribution) | Window up, daemon running; share ok through X11 capture; tray and notifications limited with each step (`apt install dunst`...). |

## Still needs a real install or the owner

- A real phone share on sway/Hyprland (view only end to end), and on Plasma 6
  Wayland (portal dialog, control, Extend without scale).
- Cinnamon, MATE and Budgie sessions (only their `XDG_CURRENT_DESKTOP` logic
  and X11 path were exercised), COSMIC, and lock/unlock on each lock screen.
- The tray icon on each panel applet, and the Thunar/Nemo/Caja menus with a
  paired phone.
