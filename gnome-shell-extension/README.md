# DeskLink GNOME Shell extension

Shows DeskLink in the GNOME top bar (GNOME 45–51) as the complete remote for a
paired phone: a Quick Settings toggle named after the phone, whose menu (and the
optional pill's, the same menu) holds everything the phone can do. It needs the
DeskLink app running; while it is not, nothing is shown.

```
Header            name · "Connected · Wi-Fi · 12 ms · charging" · battery ring
Phones            one chip per phone, with two or more
Cards             Not connected (Try again / Reconnect) · a call (Answer, Decline, Mute, End)
Quick actions     Files · Clipboard · Ring · Lock · Screen · Photo (pills, three to a row; hidden while away)
Now playing       one row: title, previous · play/pause · next; the title opens the timeline
                  (drag or click to seek; dimmed, with the reason, for a stream)
Now               only while something is going on: files moving (with their lane) · Files sync ·
                  the phone's timers · a phone using this computer, with End
More              every area, folded; open it, pick one, Back returns
  Send & share        Send files · Send clipboard · Send copied link · Ping · recent files
  Insert from phone   Photo · Photo as a file · Scan a document · Text from a picture · QR code
  Sound               play this computer's sound there · phone as microphone · phone's sound here
  Notes & clipboard   New note · snippets · Open notes · Clipboard history · share the clipboard
  Notifications       show here · share Do Not Disturb · command alerts · pause music in calls ·
                      send this computer's notifications · Open notifications
  Calls & contacts    name callers from contacts · Contacts…
  Terminals           New shared terminal · Open terminals · Terminal access
  Screen & control    Show the phone's screen… · End sessions · Glide
  Files               Open synced files · Files page · accept photos and screenshots
  Privacy & security  unlock with the phone · lock when I walk away · keep-awake from the phone ·
                      phones may hear this mic · All settings… · Unpair…
  This computer       Keep awake · Lock & hand off · Look for the phone again
Footer            Open DeskLink · Settings · Pause / Resume notifications
```

It is kept small: as wide as the Quick Settings grid (the pill's own menu is 24em, so large
text widens it), rows on GNOME's 6/12 px rhythm with 16 px icons, and long names cut short
instead of widening it. With one phone idle it is about 250 px tall; it opens folded again
each time. `menuBlocks` in `model.js` says which rows show; a new kind of row is one entry
there and one `case` in `menu.js` (a test checks that every kind is drawn).

Switches change the app's own settings and keep the menu open. Security-sensitive
ones (terminal access, unlock with the phone, accepting photos, keep-awake from
the phone, glide, the microphone) can only be turned **off** here; the app itself
refuses to turn them on over D-Bus, and an off one shows a lock and opens DeskLink.
Unpair asks in DeskLink's window, never silently. The menu never carries file
paths, contacts or call numbers.

On the lock screen (`session-modes: ["user", "unlock-dialog"]`) it shows only
"Unlock with <phone>", and only while the app offers it: "Unlock with <phone>
nearby" is on in DeskLink's Preferences and the phone is connected on this
network. The phone must still approve each unlock.

The pill, when on, is the link glyph and the phone's battery. While the phone has something ongoing (a
timer, a download, navigation) the pill appears even when it is otherwise off and shows the first item's
live countdown or progress instead, cut to 16 characters (`Activities` property; the
text is worked out by the helpers in `extension.js`, tested with `node --test gnome-shell-extension/tests/activity.test.mjs`).

While a phone is viewing or controlling this screen, the "Compiz windows effect" extension's wobble is taken
off a window the moment it appears (`WobbleGuard` in `extension.js`, tested by `tests/wobble.test.mjs`): when a
window is grabbed (drag, resize), when a maximised or tiled window is dragged out or unmaximised
(`size-change`) and when one is maximised (`size-changed`). With the wobble on, GNOME's screen cast shows
such a window as missing for the whole effect (gnome-shell #7116), so it vanished from the phone's picture. The
wobble returns when the phone stops looking. Other window effects (magic lamp, burn my windows) are not touched.

The extension only talks to the app over D-Bus
(`derx06.desklink.com.Shell1` at `/derx06/desklink/com/Shell`, see
`src/shell_dbus.rs`). Files to send are picked in DeskLink's own dialog.

Interface version 9 adds to `Devices[]` the per-phone `settings` (the switches),
`sound`, `rttMs`, `unread`, `textCapture` and `transfers`; to `Desk` the
computer-wide switches; the methods `SetSwitch(device, key, on)` (an allow-list;
the security keys refuse `on`) and `SendLink(device, link)`; and the actions
`ping`, `send-link`, `show-screen`, `media-prev`, `insert-photo-file|scan|text|qr`,
`open-files|notes|clipboard|snippets|notifications|contacts|terminals|settings` and
`unpair`. An action is listed only when the phone takes it. An older app (interface
below 9) simply shows fewer rows; below 10 it has no timeline. Interface 10 adds `media.positionMs|lengthMs|seekReason`, the `media-seek` action and the `SeekMedia(device, position_ms)` method. `model.js` has no GNOME imports and says what the
menu holds; `tests/model.test.mjs` also checks that every action and key it names is
in `shell_dbus.rs`. Run the tests with `node --test gnome-shell-extension/tests/*.test.mjs`.

## Checking it on the real top bar

The tests and a headless GNOME Shell 50 render the menu, but only the real session
shows what it looks like on your screen. After `make install` and logging in again:

1. Open the toggle's menu with one phone connected, then with the phone off: the
   header line, tiles and the Reconnect card.
2. Open More and each area; turn a switch on and off and see that the menu stays open
   and the app's Settings page agrees. Try a locked row: it should open DeskLink.
3. Light and dark style; a small screen (the body should scroll, the footer stay).
4. Start a file transfer and look for its lane and the progress bar.

## Install locally

```sh
cd gnome-shell-extension
make install          # gnome-extensions pack + gnome-extensions install --force
gnome-extensions enable desklink@derx06.github.io
```

On Wayland the Shell only picks up a newly installed extension after you log
out and back in. On X11, Alt+F2 `r` restarts the Shell instead.

Distribution packages built with Meson install it system-wide to
`/usr/share/gnome-shell/extensions/`. The app also carries a copy: its
"Turn On" button (Preferences → Appearance → Top-bar menu, or the one-time
notification on GNOME without a tray host) writes it to
`~/.local/share/gnome-shell/extensions/` when missing or outdated, then sets
`org.gnome.shell disable-user-extensions=false` and adds it to
`enabled-extensions`. Nothing changes until that button is clicked.

Logs: `journalctl -f -o cat /usr/bin/gnome-shell`.

## Publish on extensions.gnome.org

1. Bump `version-name` in `metadata.json` if you use one (EGO assigns the
   numeric `version` itself) and keep `shell-version` to versions you tested.
2. `make pack` → `desklink@derx06.github.io.shell-extension.zip`.
3. Sign in at <https://extensions.gnome.org/upload/>, upload the zip and accept
   the GPL-2.0-or-later licence terms.
4. Wait for review (see the review guidelines at
   <https://gjs.guide/extensions/review-guidelines/review-guidelines.html>):
   nothing may be created before `enable()`, and `disable()` must undo
   everything.
