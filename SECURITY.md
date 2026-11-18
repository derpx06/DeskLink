# Security

## Reporting a vulnerability

Please report security problems privately through GitHub:
**Security → Report a vulnerability** on
<https://github.com/derpx06/DeskLink>. Do not open a public issue.

Include the DeskLink version, how you installed it, and steps to reproduce.
You should get a reply within a week. Fixes are released as soon as they are
ready, and the advisory is published once users can update.

## Supported versions

| Version | Supported |
|---|---|
| 0.3.x (beta) | Yes |
| < 0.3 | No |

The relay and TURN are compiled off in 0.3 (LAN only); the relay and TURN sections
below describe a build with them on.

## What is protected, and how

- **Pairing** pins each device's certificate. A connection whose certificate
  does not match the stored pairing record is refused before any packet from
  it is handled.
- **Features** (screen, input, terminals, files, clipboard, notifications)
  travel only over WebRTC data channels and media tracks, encrypted with
  DTLS/SRTP and authenticated against the paired certificates.
- **Remote control** of the desktop needs your approval in the GNOME remote
  desktop prompt. An optional session password additionally guards screen
  viewing, control and terminals.

## Where connections may come from

DeskLink's listener (TCP 1716-1764) serves **local sources only**: private (RFC 1918), link-local, IPv6
unique-local and loopback addresses, and carrier-grade/mesh-VPN space (100.64.0.0/10, so a phone hotspot or
Tailscale keeps working: the phone app accepts the same range). A connection from any other address is dropped
before a byte is read, so a host that can route to this computer (a port forward, a routed VPN, a hotel
network) cannot raise a pairing prompt or use the unpaired slots. The rule is the same on the phone and pinned
on both sides by a shared list of addresses. Two consequences are worth knowing. **CGNAT space (100.64.0.0/10) is accepted** because Tailscale and some phone
hotspots use it and the phone app accepts the same range; the downside is that anyone else on such a network
(a tailnet member, including a node shared in, or a neighbour when this computer sits directly on a carrier-NAT
interface such as a cellular tether) can reach the listener, where they still need the pairing code you confirm.
**Loopback is accepted in release builds** (the phone emulator's proxy and local tests use it): a tunnel that
terminates on this computer, such as an SSH port forward, appears local. Unpaired links are also capped per network (six per /24, or per /64
for IPv6) as well as per address, so one host with IP aliases cannot fill the table and keep a real phone out of
pairing.

## Where the secrets are kept (read this before relying on "encrypted at rest")

- The **phone's** private key is kept in the app's private storage (SharedPreferences), not in the Android
  Keystore; backup is disabled and the app's data is excluded from device transfers. Root, a debuggable build
  or a forensic copy of an unlocked phone can read it. Moving it into the Keystore would change the key type the
  pairing certificate was made with, which pairing records must survive: it needs an explicit, versioned
  migration, and is a follow-up.
- The **desk's** notes, snippets and contacts are sealed with AES-256-GCM, but the random key sits beside the
  sealed files (`<dir>/key`, 0600). Any backup of that directory (rsync of your home, a cloud-synced folder, a disk
  image) carries both. The sealing protects against casual reading and against damaged files; it is **not**
  encryption at rest. Use full-disk encryption for that.

## The Flatpak is not a sandbox

The Flatpak build asks for `--talk-name=org.freedesktop.Flatpak` because
shared terminals must start the user's shell **on the host**
(`flatpak-spawn --host`). That permission lets the app run any command on the
host, so the Flatpak gives no isolation from the rest of your session: treat
it as a packaging format, not a confinement. (Flathub rejects this permission;
a store build would have to drop terminals.) Pictures, PDFs and videos a phone
sends are decoded inside DeskLink with a size bound, never trusted, but they
share DeskLink's address space either way.

### Flatpak permissions, one by one

`flatpak/check-manifest.py` fails CI when the manifest asks for a permission that is not in this list.

| Permission | Why |
|---|---|
| `--share=network` | The LAN link to the phone, discovery, the optional relay and TURN. |
| `--share=ipc` | GTK/X11 shared memory. |
| `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` | The window and hardware-accelerated video decode of the phone's screen. |
| `--talk-name=org.freedesktop.Flatpak` | Shared terminals start the shell on the host (see above). The one permission that removes the sandbox. |
| `--system-talk-name=org.freedesktop.login1` | Unlock with the phone and keep awake while a phone is connected. |
| `--talk-name=org.freedesktop.ScreenSaver`, `--talk-name=org.gnome.ScreenSaver` | Lock the screen from the phone and see its lock state. |
| `--talk-name=org.mpris.MediaPlayer2.*`, `--own-name=org.mpris.MediaPlayer2.DeskLink.*` | Media players on this computer show on the phone (talk), and the phone's media appears in the desktop's media controls (own). Without them a sandbox sees no players and the desktop sees no phone. |
| `--talk-name=org.kde.StatusNotifierWatcher`, `--own-name=org.kde.StatusNotifierItem-2-1` | The tray icon. |
| `--talk-name=org.gnome.Shell.Extensions` | The optional top-bar extension (turn on, turn off, state). |
| `--talk-name=org.a11y.Bus` | Accessibility (screen readers). |
| `--filesystem=xdg-run/pipewire-0` | Sound with the phone: its sound plays here and this computer's sound or microphone goes to it, as PipeWire streams (screen capture gets its PipeWire stream from the portal and does not need this). |
| `--filesystem=xdg-download` | Where received files land. |
| `--filesystem=~/DeskLink Bucket:create` | The Bucket folder, created on first use. |

## The update check

Once a day (and when you press Check for updates) DeskLink fetches one small signed file from the project's update
address, compiled into the build (a release build has no other source). The request is an anonymous HTTPS GET: it
carries the DeskLink version in its User-Agent and an `If-None-Match`, and no name, no device identifier, no cookie (a random install
number is kept on this computer only, to decide your staged-rollout bucket, and is never sent). The server can see your network address and the time, as any website can. The file is Ed25519-signed with
the project's key and checked under a strict profile (size, JSON, canonical base64, signature, dates, links pinned to
this project's release pages); anything else is ignored without a message. DeskLink only tells you a newer version
exists and how to update for how you installed it; it never downloads, installs or runs anything. Settings, Advanced, Check
automatically (`update_check`) turns the daily check off. A build with no update address does no checking at all: no thread, no state file.

## Where DeskLink keeps its files

`~/.config/desklink`, `~/.local/state/desklink` and `~/.cache/desklink` are
created `0700` and their files `0600` (older versions made them readable by
other users; DeskLink tightens them once at start).

## Update check

DeskLink fetches the update server's `latest.json` over HTTPS about once a day, and when you open the app
if the last answer is old. The request carries no identifier and no account, no cookies and no query
string (a random install number is kept on this computer only, to decide your rollout bucket, and is never sent); only DeskLink's version number, in the `User-Agent`. The server operator (Firebase Hosting) can see
your network address and the time, as any website can. The file is signed (Ed25519, public keys compiled
into DeskLink); anything unsigned, tampered, expired, older than one already seen, or with an address
outside the project's own is ignored. A manifest is a report that a version exists: it cannot run a
command or install anything, and DeskLink only shows an update window when you open the app. Builds have no
way to point the check elsewhere (only debug builds read an address override). Turn it off in Settings >
About; with it off no update request is made.

## What the relay can and cannot see

The relay ([`relay/`](relay/)) only helps two paired devices find each other
across networks.

It **cannot** see:

- The content of the connection setup it forwards. Messages are end-to-end
  encrypted (ECDH, HKDF, AES-GCM) and signed with the devices' pairing keys.
  This includes the SDP and ICE candidates, and so your devices' local
  addresses.
- Anything you use DeskLink for. Screen video, input, files, clipboard,
  notifications and terminal data never pass through the relay.

It **can** see:

- Each connected device's certificate fingerprint (a SHA-256 hash), and which
  fingerprints each device allows, that is, which devices are paired.
- When paired devices are online, and the size and timing of the messages
  they exchange.
- Client IP addresses. They are kept in memory for rate limits only; the
  daily quota file stores salted hashes, and the salt changes every UTC day.
- Requests for TURN credentials.

If no direct path exists and TURN is allowed, encrypted media may flow through
a TURN server (Cloudflare Realtime or a self-hosted coturn). It sees IP
addresses and traffic volume, not content. TURN is used exactly when the relay is
(`RELAY_ENABLED`, off in 0.3); there is no setting for it.

Running your own relay: [`relay/deploy/DEPLOY.md`](relay/deploy/DEPLOY.md).
