# Changelog

## 1.3.0 — 2026-10-25

- Keep awake is off for new installs and holds the computer awake only while a
  paired phone is connected (it used to hold for as long as any phone was
  paired).
- One cleanup for every unpair, from either side, replaced phone or changed
  identity: addresses, sync state, liveness, per-phone choices and open
  sessions are forgotten together.
- Pairing by code closes its dialog, opens the new phone and says so.
- The empty notifications page points at notification access on the phone.
- Computer-wide switches on a phone's page say they apply to every phone.
- The GNOME top-bar menu can be turned off again; the row says what turning
  it on changes.
- Hardening from the audit: config forward-compatibility, notes ancestors,
  bounded image decoding, private file modes, session-password binding.
- Version 1.3.0 matches the Android app (1.3.0, versionCode 20300).

## 1.0.0 — 2026-10-19

First public release.

- Phone screen on the desktop, with hardware H.264 where the phone supports it.
- Remote control of the computer's mouse and keyboard from the phone, through
  the GNOME remote desktop portal and only after you approve it.
- Shared terminals between phone and desk, with one tap to start Claude Code
  or Codex, and a terminal window at the desk for each session.
- Phone notifications on the desktop, with replies and actions.
- Clipboard sync in both directions; text and link sharing.
- File transfer both ways, and a browser for the phone's files.
- Presentation remote, ping, find-my-phone, battery status, and locking the
  desktop session from the phone.
- Away from home through the DeskLink relay, with end-to-end encrypted
  signaling and optional TURN.
- Optional session password for screen and terminal access, stay awake while
  a phone is paired, a tray item to end phone sessions, and 30 days of
  connection history.
- Pairing by QR code with a six-digit verification code. No account.
