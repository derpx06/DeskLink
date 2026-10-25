# Flatpak

The manifest is `../derx06.desklink.com.json`. It builds offline from a git tag:
crates come from `cargo-sources.json`, generated from `Cargo.lock`. The
`relay/` crate is a path dependency inside the checkout, so it needs no entry.

The runtime is GNOME 51 because DeskLink needs GStreamer 1.28 (`gstreamer`
feature `v1_28`); GNOME 49 and 50 ship 1.26.

## Generate `cargo-sources.json`

Regenerate it whenever `Cargo.lock` changes. It is not committed here (it is
git-ignored); it lives next to the manifest in the Flathub repository.

```sh
git clone https://github.com/flatpak/flatpak-builder-tools
pip install --user aiohttp tomlkit      # the generator's only dependencies
python3 flatpak-builder-tools/cargo/flatpak-cargo-generator.py \
    Cargo.lock -o flatpak/cargo-sources.json
```

## Build locally

```sh
flatpak install flathub org.gnome.Sdk//51 org.freedesktop.Sdk.Extension.rust-stable//26.08
flatpak-builder --user --install --force-clean build-flatpak derx06.desklink.com.json
flatpak run derx06.desklink.com
```

Use the `rust-stable` branch that matches the runtime's freedesktop base
(the `GL` extension `versions` in `flatpak info -m org.gnome.Platform//51`). To build the working tree instead
of the tag, temporarily replace the git source with
`{ "type": "dir", "path": "." }`.

## Submitting to Flathub

1. Tag the release: `git tag -s v1.3.0 && git push origin v1.3.0`.
2. Add `"commit": "<full sha of v1.3.0>"` next to `"tag"` in the manifest.
   Flathub requires both. JSON has no comments, so this file is the reminder.
3. Set `DESKLINK_RELAY_URL` in the manifest's build `env` to the deployed
   relay (`relay/deploy/DEPLOY.md`). Release builds always set it: it is baked
   in as the default relay, and the manifest's `wss://relay.desklink.example/v1`
   is the same placeholder the source falls back to without it.
4. Generate `cargo-sources.json` from that tag's `Cargo.lock` (above).
5. Make sure the screenshot URLs in the metainfo resolve
   (`data/screenshots/*.png` on `master`; see the top-level README).
6. Fork <https://github.com/flathub/flathub>, branch from `new-pr`, add the
   manifest and `cargo-sources.json`, and open a pull request against
   `new-pr`. Follow <https://docs.flathub.org/docs/for-app-authors/submission>.
7. Run the linter before submitting:
   `flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest derx06.desklink.com.json`
   and `... repo repo` on the built repository.

### Expected review questions

- **App ID.** `derx06.desklink.com` is frozen (installed, persisted and
  D-Bus-activated; see `docs/IDENTIFIER_COMPATIBILITY.md` in the workspace).
  It is not a verifiable reverse-DNS name, and `appstreamcli` reports
  `cid-maybe-not-rdns`. Flathub may ask for a different ID such as
  `io.github.derpx06.DeskLink`. Changing it is a product decision with a
  migration cost, not a packaging fix. Decide this before submitting.
- **`--talk-name=org.freedesktop.Flatpak`** lets the app run host shells
  through `flatpak-spawn --host` for shared terminals. Justify it in the PR.
- **`--system-talk-name=org.freedesktop.login1`** holds the stay-awake
  inhibitor and locks the session on the phone's request.
- **ScreenSaver names** are used to lock the screen.
- **`org.kde.StatusNotifierWatcher`** registers the tray item under
  `org.kde.StatusNotifierItem-<pid>-1`; the app is PID 2 in the sandbox, hence
  `--own-name=org.kde.StatusNotifierItem-2-1`. Without it the item falls back
  to the connection's unique name.
- **`--talk-name=org.gnome.Shell.Extensions`** offers (once, on GNOME without
  a tray host) to enable or install the DeskLink top-bar extension. GNOME
  shows its own confirmation.
- **`--talk-name=org.a11y.Bus`** reaches the accessibility bus (its address
  and the `org.a11y.Status` properties) to watch which text field has focus,
  so the phone's keyboard opens when the desktop expects typing.
- **`--filesystem=xdg-download`** is where received files are saved.
- Notifications, screen casting and remote input go through portals and need
  no permission.

## Known sandbox caveats

- **Shared terminals run on the host.** Inside the sandbox (`/.flatpak-info`
  exists) the app starts shells and agent CLIs with `flatpak-spawn --host
  --watch-bus setsid --ctty --wait`, so the phone gets the user's real login
  shell with job control, and asks the host's login shell which agents are
  installed. The desk terminal window is the host's terminal app running
  `flatpak run --command=desklink derx06.desklink.com --attach-terminal <id>`;
  the session socket lives in `$XDG_RUNTIME_DIR/app/derx06.desklink.com/`, which
  every instance of the app shares (`src/device_links/pty/host.rs`). This is why
  the manifest needs `--talk-name=org.freedesktop.Flatpak`.
- **Separate state.** The sandboxed app keeps its configuration, pairing
  records and certificate under `~/.var/app/derx06.desklink.com/config/desklink/`,
  not `~/.config/desklink/`. Phones paired with a non-Flatpak install have to
  pair again.
