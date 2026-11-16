# Top-bar menu redesign (STATE row 3.8)

Audit of the 1.3 menu (screens in `.tools/screenshots/p38-topbar/`): the structure was sound (stock Quick
Settings header, chips, pill tiles, More), but

1. "what is happening now" sat under the tiles and the music, so a transfer or a live session was below the fold;
2. six small pill tiles carried Lock and Photo, which are rarely wanted, and the useful four were not large;
3. the header showed a phone glyph whether or not its screen was shared;
4. the offline card said "Not connected" with no next step; the empty state was a dim sentence and an Open row;
5. chips showed only a dot: no connecting state, no unread count, no live mark;
6. the panel glyph had a coral dot for a live session only: unread notifications and transfers were invisible;
7. the optional pill stayed on the bar (empty chrome) whenever a phone was paired, connected or not.

## New layout (ASCII)

```
[ (phone)  Pixel 9                (82) ]   header; glyph becomes the screen while shared
[   Connected · Wi-Fi · 12 ms        ]
[ (o) Pixel 9 · 3   ( ) realme  (o) ]    chips: mint = connected, amber = connecting, hollow = away,
                                          count = unread, coral dot = using this computer
[ call card: Answer / Decline        ]   only while ringing / talking
[ Receiving holiday.mov   42% ====   ]   NOW (only while something is): transfers, sync, timers,
[ (o) Pixel 9  Viewing · 2 min  End  ]   phone sessions with End
[ Files ][ Clipboard ][ Screen ][ Ring ] four large tiles (icon over label)
[ music row, timeline on request     ]
[ More                      3 new  > ]   everything else; Lock, Photo, Insert, Sound, Privacy ...
[ Open DeskLink | Settings | Pause   ]
```

No phone: "No phone yet" / one sentence on how to pair / an "Open DeskLink to pair" button (stock button: the accent
with white text is 3.9:1 on some accents, so no custom primary style).
"Now" rows are capped at two (sessions first, then files, sync, timers) with a "+N more · in DeskLink" row,
so the tiles and More stay reachable on a short screen.
Offline: card with the reason in plain words ("Open DeskLink on <phone> and keep it on the same Wi-Fi as
this computer.") and Try again. The Quick Settings tile stays as the entry point and says "Not connected".

## Panel
- Glyph in the Quick Settings indicator: coral dot = a phone uses this computer; accent dot = unread
  notifications or a file moving; nothing when quiet. Words in the accessible name
  ("Pixel 9 connected, 3 new notifications"). GNOME gives panel icons no hover text, so the accessible name
  is the tooltip equivalent.
- The optional pill appears only while a phone is connected and it has something to show; offline or no
  phone leaves no empty chrome. It hides after 300 ms of being unwanted and appears at once (no jump on a flap).

## Data and logic
All in `model.js` (tested in `tests/model.test.mjs`): `quickTiles` (4), `menuBlocks` order, `headerGlyph`,
`offlineHint`, `EMPTY`, `chipState`, `panelBadge`, `pillVisible`, `shortStatus`. No `shell_dbus` change:
`unread`, `transfers`, `screen` were already in `Devices[]`; no thumbnail is sent (a glyph is used).
Interface version stays 10; an older app that lacks a field simply shows fewer marks.

## Native feel
Stock Quick Settings header, accent colour (`-st-accent-color`) for the primary button, accent dot, checked
chip; symbolic icons only; no fixed pixel widths; rendered at 100 %/200 %, light/dark, large text and RTL.
Notifications and the call banner keep the 3.3 card (app icon, title, one paragraph, buttons under it).

## Checked in batch 2 (headless Shell, `.tools/screenshots/p38-topbar/`)
- High contrast (dark and light): tiles, chips, cards, buttons keep their borders and focus rings.
- Keyboard (`keyboard/`): Tab goes End -> "+N more" -> the tiles in order, each with the accent ring; Escape closes;
  the pair and Try again buttons take focus.
- Every area of More (`all-areas/`), including the new "Lock the phone" row.
- Older app: a device without `actions`, `settings`, `unread`, `transfers` or `rttMs` draws what it has (the
  tiles greyed, no switches, no latency) and nothing throws (`an old app's bare device still gives a menu`).
- Notifications and calls are GNOME's own message cards (one source per app and phone, so they group; the
  banner expands to the full paragraph and the buttons); Answer/Decline appear only when the app offers them.
