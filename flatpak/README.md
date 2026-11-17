# Flatpak

The manifest is `../derx06.desklink.com.json`. It builds offline from a git tag:
crates come from `cargo-sources.json`, generated from `Cargo.lock`. The
`relay/` crate is a path dependency inside the checkout, so it needs no entry.

The runtime is GNOME 51 (GTK 4.24, libadwaita 1.10, GStreamer 1.28: the
`webrtcbin` the file-transfer frame size was measured against). The build
floor alone would allow GNOME 49 or 50 (GStreamer 1.26). The runtime has
every GStreamer element DeskLink uses except `ximagesrc`, so X11 sessions
without a screen-sharing portal cannot share the screen from the Flatpak
(the System check says so).

The GNOME runtime has no VTE, so the manifest builds it (GTK 4 only, the
`vte` module) for the Terminals page's interactive terminal. Its release
tarball carries its meson subprojects (simdutf, fmt, fast_float), so it
builds offline. Bump it with the runtime: the archive URL and `sha256`.

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

With the `org.flatpak.Builder` app instead of a host `flatpak-builder`, pass
`--env=FLATPAK_USER_DIR=$HOME/.local/share/flatpak` to `flatpak run`: the app's
own user installation is elsewhere, and the `rust-stable` extension installed
with `--user` is not found (`rustc` missing in the desklink module).

Use the `rust-stable` branch that matches the runtime's freedesktop base
(the `GL` extension `versions` in `flatpak info -m org.gnome.Platform//51`). To build the working tree instead
of the tag, temporarily replace the git source with
`{ "type": "dir", "path": "." }`.

## Releasing, and Flathub

The GitHub release workflow builds the bundle itself: it swaps the manifest's
git source for the checked-out tree (`flatpak/ci-manifest.py`, which picks the
`desklink` module by name) and runs
`flatpak/check-manifest.py --release --commit "$GITHUB_SHA"`, so the tag's own
commit need not be written in the manifest. Try the same locally:
`python3 flatpak/ci-manifest.py` writes `ci-manifest.json`.

The Flatpak is **not planned for Flathub**: shared terminals need
`--talk-name=org.freedesktop.Flatpak`, which Flathub rejects (SECURITY.md). If
that changes, a submission needs:

1. A signed tag: `git tag -s v0.3.0 && git push origin v0.3.0`.
2. `"commit": "<full sha of the tag>"` next to `"tag"` in the manifest
   (`flatpak/check-manifest.py --release` checks it).
3. The relay address: the manifest sets none and the source falls back to an
   empty default (no default relay). A build with the relay enabled sets
   `DESKLINK_RELAY_URL` in the manifest's build `env`.
4. `cargo-sources.json` generated from that tag's `Cargo.lock` (above).
5. Screenshots: the metainfo has none today; add `data/screenshots/*.png` and
   the `<screenshots>` element together.
6. A pull request against <https://github.com/flathub/flathub> (branch from
   `new-pr`), following <https://docs.flathub.org/docs/for-app-authors/submission>.
7. The linter first:
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
- **`--filesystem=xdg-run/pipewire-0`** is sound with the phone: DeskLink
  plays and captures audio as PipeWire streams, and the sandbox has no
  PipeWire socket without it (`desklink --check` says so).
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
