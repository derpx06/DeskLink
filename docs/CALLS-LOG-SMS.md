# Call log and SMS: opt-in, non-Play builds only

DeskLink ships on Google Play. Play restricts `READ_CALL_LOG`, `READ_SMS`,
`SEND_SMS` and `RECEIVE_SMS` to apps that are the default dialer or SMS app,
so the Play build must never declare them. What the Play build does instead:

- Recent calls are the numbers dialled from the computer (`dial_recents.json`
  in the desk's config dir, newest first, eight at most, cleared from the dial
  pad). The phone's call log is never read.
- "Message" opens the phone's own composer (`desklink.telephony.request`
  `message`). Nothing is read or sent by DeskLink.
- Missed calls come from the line state (no log), with the name from the
  phone's shared contacts.

## Opt-in design for Flathub / AUR / sideload APK

A separate build flavour (`full`), never uploaded to Play, declares the
permissions in a flavour manifest and sets `BuildConfig.CALL_LOG_SMS = true`.

- Capabilities: `desklink.calllog` (snapshot of the last 50 calls: number,
  name, type, time, duration, with a `rev` for delta resync) and
  `desklink.sms` / `desklink.sms.request` (threads, last messages, send).
  Advertised only when the flavour has the handler and the person turned on
  "Share call history" or "Messages" on the phone (off by default, per phone).
- Same rules as every feature: WebRTC only, constants identical on both sides,
  dispatch on packet type, peer data is a report. A send is an explicit
  request that the desk confirms and that the phone, not the packet, decides;
  the body is capped, and a number goes through the same refusal rules
  (emergency, `*`/`#`).
- Desk: a Calls tab with the log and call-back, a Messages page. Notifications
  for an incoming SMS honour the lock-screen privacy switch.
- Verification stays manual: Play's policy check does not apply to these
  builds, but their listing must say what they read.
