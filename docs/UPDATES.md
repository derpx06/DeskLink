# Updates

DeskLink tells you when a newer version exists and how to install it for the way you installed
DeskLink. It never downloads or installs anything by itself.

## For people using DeskLink

**When you see it.** When you open DeskLink (start it, or bring its window back from the tray or the
top bar), a small window may say "DeskLink 0.4.0 is available". It appears at most once per time you
open the app, and never while another dialog, a pairing prompt or a file offer is on screen. Choose
**Remind me tomorrow**, **Skip this version**, or **How to update**. If it was only dismissed, the
GNOME top-bar menu keeps one quiet row, "Update available", with a small dot on the icon; the row
opens the same window. There are no update notifications.

**What it checks.** Once a day while DeskLink runs (and when you open it, if the last answer is old)
it asks DeskLink's update server for one small file listing the newest version. The file is signed by
the project; DeskLink ignores anything that is not signed or is out of date or is older than one it
has already seen. Nothing in it can run a command: it can only show text and a link you choose to open.

**What it sends.** One ordinary HTTPS request. It carries no name, no device ID, no pairing data and
no account, and no cookies. The only thing about you in it is DeskLink's version number, in the
`User-Agent` line. The server can see your network address and the time, as any website can.

**Turn it off.** Settings > About > Updates > **Check automatically**. With it off, DeskLink makes no
update request at all; **Check for updates** still works when you press it. A build that was made
without an update address shows "Updates aren't set up in this build" and does nothing.

**How to update** (the window shows the right one for your install):

| You installed with | Update with |
|---|---|
| Flatpak | `flatpak update derx06.desklink.com` |
| Arch (AUR) | `yay -Syu desklink` (or `desklink-bin`, or your AUR helper) |
| Debian, Ubuntu | `sudo apt update && sudo apt install --only-upgrade desklink` |
| Fedora | `sudo dnf upgrade desklink` |
| openSUSE | `sudo zypper refresh && sudo zypper update desklink` |
| AppImage | download the new AppImage from the release page and replace the file |
| a download or `cargo` | download the new version from the release page |
| Nix | update through your Nix configuration |

DeskLink shows the command and a **Copy command** button; it never runs it. Your distribution may not
have the new version yet.

**Beta.** DeskLink 0.x is in beta, so the update channel is **Beta**. **Stable** becomes selectable
when 1.0 is out. A beta offer is labelled "(beta)".

**"Update needed".** Rarely, a version can no longer connect to a newer phone or has a serious
security fix. The window then has only **How to update** and returns at every open until you update.
It never traps you: Escape closes it for that open, and pairing and transfers keep working.

## For developers

The design is `docs/UPDATES.md` in the workspace root (the owner-approved source of truth); this is the
short version for running it.

**Code.** `src/update/` (leaf module: model, keys, fetch, store, install source, schedule),
`src/window/updates/` (popup, Settings group), the top-bar properties `update_available`,
`update_version`, `update_required` in `Desk` and the `ShowUpdate()` method in `src/shell_dbus/`, and the
row and dot in `gnome-shell-extension/` (`updateInfo` in `model.js`, tested by `tests/model.test.mjs`).
Nothing outside those calls into the update code.

**Run each state against a debug build.** Debug builds only accept `http://127.0.0.1` and trust the
labelled test key; release builds have no override. Always use a throwaway profile, never your live one.

```sh
# 1. the test server (repo desklink-updates), one scenario at a time
python3 scripts/serve-update-manifest.py --scenario available --port 8099
#    scenarios: uptodate available available-beta required skip rollout-0 rollout-50 expired
#    replayed badsig tampered oversized slow 500 404 redirect badjson future-schema no-notes
#    flip one live: curl 'http://127.0.0.1:8099/control?set=required'

# 2. the desk, on its own profile
export XDG_CONFIG_HOME=$(mktemp -d) XDG_STATE_HOME=$(mktemp -d) XDG_CACHE_HOME=$(mktemp -d)
DESKLINK_DEBUG=1 DESKLINK_UPDATE_URL=http://127.0.0.1:8099/latest.json cargo run
#    the log line "update-state ..." shows what was decided
```

A fresh profile has no paired phone and a new identity, so it cannot disturb a real one; do not copy
`~/.config/desklink`. For the top bar, use a headless GNOME Shell with the fake service in
`.tools/overnight/topbar-harness` (scenario names ending in `+update`, `+required`, `+hostile`); never
the live Shell.

**Publish an update.** The owner's one-time setup and the deliberate publish step are in
`docs/update/OWNER-SETUP.md` (in the `desklink-updates` repo). Releases set `DESKLINK_UPDATE_URL` the
same way as `DESKLINK_RELAY_URL`; an empty value makes the feature inert.

**Changing the top-bar extension.** Any change to a file in `gnome-shell-extension/` changes the
extension digest: add the new one to `KNOWN_BUILDS` in `src/shell_dbus/extension_files.rs` (the test
`cargo test extension_files` fails until you do).
