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
| 1.0.x | Yes |
| < 1.0 | No |

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
| `--talk-name=org.kde.StatusNotifierWatcher`, `--own-name=org.kde.StatusNotifierItem-2-1` | The tray icon. |
| `--talk-name=org.gnome.Shell.Extensions` | The optional top-bar extension (turn on, turn off, state). |
| `--talk-name=org.a11y.Bus` | Accessibility (screen readers). |
| `--filesystem=xdg-download` | Where received files land. |
| `--filesystem=~/DeskLink Bucket:create` | The Bucket folder, created on first use. |

## Where DeskLink keeps its files

`~/.config/desklink`, `~/.local/state/desklink` and `~/.cache/desklink` are
created `0700` and their files `0600` (older versions made them readable by
other users; DeskLink tightens them once at start).

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
addresses and traffic volume, not content. Turn TURN off under Preferences →
Away From Home to allow direct paths only.

Running your own relay: [`relay/deploy/DEPLOY.md`](relay/deploy/DEPLOY.md).
