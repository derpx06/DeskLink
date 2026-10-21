# DeskLink GNOME Shell extension

Shows DeskLink in the GNOME top bar (GNOME 45–51): a pill with the link glyph
(computer, wire, phone; green while a phone is connected) and a coral dot while
a phone is using this computer. Its menu lists what is live now, each with
End, then End Everything, the desk's tiles (Keep Awake, Clipboard, Send Files,
Ring Phone), Lock & Hand Off and Open DeskLink. It needs the DeskLink app
running; while it is not, nothing is shown.

On the lock screen (`session-modes: ["user", "unlock-dialog"]`) it shows only
"Unlock with <phone>", and only while the app offers it: "Unlock with <phone>
nearby" is on in DeskLink's Preferences and the phone is connected on this
network. The phone must still approve each unlock.

The extension only talks to the app over D-Bus
(`derx06.desklink.com.Shell1` at `/derx06/desklink/com/Shell`, see
`src/shell_dbus.rs`). Files to send are picked in DeskLink's own dialog.

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
