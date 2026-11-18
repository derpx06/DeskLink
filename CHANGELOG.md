# Changelog

## 0.3.0 (beta)

The first public release. It works and is still being shaped: expect rough
edges and please report them. LAN only: the away-from-home relay is built but
switched off.

- Update check (`src/update/`, `src/window/updates/`): once a day DeskLink fetches one signed manifest to learn whether a newer version exists and shows a popup when you open it, with how to update for the way you installed it (a command to copy, or the download page). It never downloads or installs anything and its request carries no identifier (a random install number is kept on this computer only, to decide your staged-rollout bucket, and is never sent); it does nothing in a build without an update address. Settings, Advanced has Check for updates, Check automatically, the channel and Skipped with Undo. Top-bar interface 11 adds the read-only `update_available`, `update_version`, `update_required` and `ShowUpdate()`.

**What it does**

- Phone screen on the desktop, with hardware H.264 where the phone supports it.
- Remote control of the computer's mouse and keyboard from the phone, only after you approve it.
- Shared terminals between phone and desk (the phone can start Claude Code or Codex), and a terminal
  window at the desk for each session.
- Phone notifications on the desktop, grouped by app, with replies and actions; clipboard sync both ways; text and link sharing.
- File transfer both ways, and a Bucket folder kept in step; contacts and calls on the computer; notes and
  snippets; media control both ways; the phone's microphone or sound here and this computer's sound there; Glide.
- Pairing by QR code or a six-digit code, no account. Optional session password, stay awake while a phone is
  paired, and 30 days of connection history.
- "Find anything" (Ctrl+K): one search over the actions, pages, phones, terminals, recent files, notes, snippets,
  clipboard history, contacts and settings.
- The GNOME top-bar menu is the complete remote for a paired phone (what is happening now first, four large
  tiles, plain offline and empty states). Top-bar interface 10.

**Connection**

- A ready phone that goes silent shows "Checking connection…" on every surface after 4 s; the desk probes after
  2 s and decides on corroborated evidence (8 s on both paths); the phone tells the desk when it is about to leave
  (Wi-Fi off, airplane mode, service end, network lost); a click on a quiet phone checks first and says
  "Can't reach <phone>" at once; background sends wait for the phone instead of being lost.
- Pairing from the phone now brings the connection up by itself; an unpair the phone never heard is answered
  the next time it connects.
- A finished transfer whose completion was lost is answered, not received twice.

**Look and feel**

- Home: a Notes card with the newest notes, New note and quick-add; two balanced columns on a big window;
  Everything DeskLink does in two columns; a calm Home while the phone is away.
- A narrow sidebar rail between the overlay and 1000sp; Files, Notes, Terminals, Contacts and Settings use a big
  window (the chosen contact, or the page list, beside the open one).
- Keep awake is off for new installs and holds the computer awake only while a phone is connected.

**Security and robustness**

- Pairing pins each device's certificate; unpaired links are capped per network; the listener serves private,
  link-local, unique-local and carrier-NAT (100.64.0.0/10, for hotspots and Tailscale) sources, and loopback;
  anything else is dropped before a byte is read. SECURITY.md says where the secrets are kept and that the
  Flatpak build is not a sandbox (shared terminals need host access). The relay's images are pinned and its TURN
  secret no longer sits on a command line. The debug-only verbs module is compiled out of release builds.
- One cleanup for every unpair, from either side: addresses, sync state, liveness, per-phone choices and open
  sessions are forgotten together.
- Hardening from the audits: config forward-compatibility, notes ancestors, bounded image decoding, private file
  modes, session-password binding, thumbnail markers, log caps.
- The Flatpak, CI and releases: checks run before a release is built, actions are pinned, `deny.toml` checks
  licences and advisories, THIRD_PARTY_LICENSES ships with the app.

**Not included** (removed before this release): browsing the phone's files, the presentation remote, Bluetooth,
run commands, automations, the drawing pad, the webcam and system volume.

Version 0.3.0 matches the Android app (0.3.0, versionCode 20300).
