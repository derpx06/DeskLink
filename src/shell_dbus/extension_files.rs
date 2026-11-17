//! The extension's files embedded in the app: their digests, the builds known
//! to be ours, and installing them atomically.
//!
//! Sibling edges: none (the parent's `EXTENSION_UUID` and
//! `user_extension_dir`).

use super::*;

/// The extension, built into the app so any install can turn it on.
pub(super) const EXTENSION_FILES: [(&str, &str); 7] = [
    (
        "metadata.json",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/metadata.json"),
    ),
    (
        "extension.js",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/extension.js"),
    ),
    (
        "notifications.js",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/notifications.js"),
    ),
    (
        "stylesheet.css",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/stylesheet.css"),
    ),
    (
        "menu.js",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/menu.js"),
    ),
    (
        "desklink-symbolic.svg",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/desklink-symbolic.svg"),
    ),
    (
        "model.js",
        include_str!("../../gnome-shell-extension/desklink@derx06.github.io/model.js"),
    ),
];

/// Whether this desktop can show the top-bar menu at all.
pub fn extension_possible() -> bool {
    // A Flatpak cannot install into the host's shell: no offer, no failure.
    !crate::device_links::pty::host::in_flatpak()
        && is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
        && gio::SettingsSchemaSource::default()
            .and_then(|source| source.lookup("org.gnome.shell", true))
            .is_some()
}

/// Beside the installed extension: the digest of what DeskLink wrote, so a
/// copy the user changed is told apart from an older one of ours.
pub(super) const INSTALL_STAMP: &str = ".desklink-installed";

/// SHA-256 over each extension file's name and contents (missing: empty).
pub(super) fn files_digest<'a>(files: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut hasher = openssl::sha::Sha256::new();
    for (name, content) in files {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(content.unwrap_or_default().as_bytes());
        hasher.update(&[0]);
    }
    hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every extension DeskLink has shipped, by [`files_digest`] (SHA-256 of
/// name, NUL, content, NUL per file) over the `EXTENSION_FILES` names, one per
/// version in the git history of `gnome-shell-extension/` (a file a build
/// lacked counts as empty). The test `a_copy_is_ours_when_stamped_or_a_shipped_build`
/// prints the digest to add. A copy installed by hand, with no stamp, is
/// still ours and safe to update.
pub(super) const KNOWN_BUILDS: [&str; 30] = [
    // One quiet update row and a dot (ShowUpdate), the narrower Checking... subtitle.
    "b02adfa5665f98d43b86b4356180c3b80e7f2d8804f1f314e4873a3fe89d0e2a",
    // Checking connection...: a ready but quiet phone says so, its tiles grey out.
    "325ddf89e082f4d55b0864c03d11ea563df2e86a81c465e81f7ae33b0d951fef",
    // Batch 2: old-app safe, offline glyph, short-screen order, tiles note, wrapped header.
    "5eafa7fc3c291f27400e8c4fc76169fd35217fa13a04dc3f43801368fccad697",
    // Top-bar redesign: what is happening now first, four large tiles, a
    // plain offline/empty state, chip and panel badges, the pill hides offline.
    "76b5364d49dd6e55fd515cff36b51ac0855e6d5be128ad55897666122d71ce4c",
    // Top-bar UI pass: rows line up, icons that exist, the footer stays on a
    // short screen, readable in the light style, focus rings.
    "885de5bb337209bbfb06918befc3b63cf70c3f89e463bab105098d0b133f22aa",
    // The compact menu: header line, pill tiles, one music row, every area in More.
    "e03918201c7c84592e19242f05859a6dcc977553a3d4423af013dedcbdda8c62",
    // The tray adds notifications oldest first, so the newest is on top (F21).
    "289b530b038d2bf88dbbb53bdd4b17dd32305fe6ed83190953b295dfc4850b92",
    // Compact menu + tray order + Ask rows, merged (desk-final5).
    "9e5776564dddcd6a1b86dc1664759b922f364bca9b70b54a6ea35828c26654cb",
    // The lane chip in the words of both apps ("Fast, over Wi-Fi").
    "8c8ad0ee0416248afe9e374ff44cb34e441c770a2d8d3679e5f4877af361910a",
    // The merged notifications + ext-seek build.
    "82bbb00882dd33f3741a1d45c86e6ba39ca2e9b32b4237ea34152c219da468c3",
    // Notifications: one plan; a link change never re-banners.
    "978339f19e09ed6c22f083944bb9a2641c2ef3c481b9d0351ab5b3f7cc305149",
    // The ext-seek build (timeline, tidier header, no clipped tiles).
    "8ca439b2ba46a297bae63f5b30f1a32204c02e67b04bb14b416a4db6e6d6db83",
    // The current build (no wobbling window while a phone looks at this screen).
    "f7b1d5832081bd42eb8aaa027e115a3963208f9490d73840539df50f829bd347",
    // Links checked, focus kept only while open.
    "32a1f784c930664df85dfce8b6572ea1a8608150585320cfb6c7fb090ac854a7",
    // The complete remote, first cut (model.js, sections, switches).
    "b1c3aeb335ad359842ef42bbc1951fe68acc047c5be6116aea83185e2ae1f737",
    // Wave 2: the ongoing-items chip in the pill.
    "c27272d96fac1f6029a7494eacbe93b716a44b05959acb0ded9b697e8f42a5d2",
    // w8: the Photo tile, `insert-photo`.
    "4d5b8192ae0a8fdcbceef31bcf955ff08d555b8cae633d758d77521e376687e4",
    // w7c: no images decoded in the shell, follow-up messages announced,
    // the reply dialog destroyed, accent fallback.
    "23025d05bb0300675bc3d5f1c046be8b267deb7f6692767003822244d1341725",
    // W4: the Files row, Settings in its comments.
    "b07fccfbb89268d72553923fe74ed53089e2042fac1fe006ede1f89ff7fd2e4e",
    // The Bucket's sync progress row.
    "9bdbd7405e48738c94bd65dfae6baa2f412c78c9f892fad0bfe5c2656858138f",
    // 35a2ff3, F4: glide in the live row.
    "1744f9ec921dbf1f1e3fb9c4215b87190518f4dbe40cd8ce21a5336150987d3f",
    // F6: Snippets and New Note in the menu.
    "d20b1b57eea99e3c781feb69b6ab5bed3393e22e0eb81d88e5b7d30af5cb88e4",
    // Top-bar redesign: menu.js, the glyph icon.
    "3413a34defbe375ac5347683de5a53f89d0df1c11e586b9b454cc43348154e77",
    // be1b760, 2026-10-21.
    "07555cf126179911873994980fc0a360b4fea38819bee64277a44ad752ab2649",
    // 9784a7d, 2026-10-21.
    "2aa899e5394ac50cb6fbbb536d135797d93bf5d8adcfa50ba130c743d956154b",
    // f798285, 2026-10-21.
    "cc953aaa3a1db690eff0f36053be389f6527da1921f1285829a0e6ced2f7e06f",
    // 1b78f08, 2026-10-21.
    "aa5bffbecce7a7ccc59b6a0e611dbd44ece3377a1ed547305266d941079d476e",
    // ba03b84, 2026-10-21.
    "d68895988634a5a62377c63ca97bd643f901f232419b4b8c84f341b20c49f4f4",
    // 635fc5d, 2026-10-20.
    "cff05ba7d6f7e20074ae6fa7406177e58ac4bf8901d07faace5f131f07da8878",
    // 1f8c0a4, 2026-10-20.
    "1fedcbddc921c5b1a5ebe46fd73127d6a732c0f67d74e78f24fa06402abbafe9",
];

/// Whether an installed copy (by digest) is DeskLink's own and may be
/// replaced: the stamp we wrote says so, or it is a build we shipped.
pub(super) fn ours(digest: &str, stamped: bool, known: &[&str]) -> bool {
    stamped || known.contains(&digest)
}

/// Writes `path` whole or not at all.
fn write_atomically(path: &std::path::Path, content: &str) -> Result<(), String> {
    let temporary = path.with_extension("desklink-tmp");
    std::fs::write(&temporary, content).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

/// Writes the extension into `dir` (`~/.local/share/gnome-shell/extensions/<uuid>`)
/// when missing or an older copy of ours. Whether anything was written. A
/// copy someone changed is left alone, with an error saying so.
pub(super) fn install_extension_files(dir: &std::path::Path) -> Result<bool, String> {
    let present: Vec<(&str, Option<String>)> = EXTENSION_FILES
        .iter()
        .map(|(name, _)| (*name, std::fs::read_to_string(dir.join(name)).ok()))
        .collect();
    let current = files_digest(present.iter().map(|(name, have)| (*name, have.as_deref())));
    let embedded = files_digest(
        EXTENSION_FILES
            .iter()
            .map(|(name, content)| (*name, Some(*content))),
    );
    let stamp = std::fs::read_to_string(dir.join(INSTALL_STAMP)).ok();
    let stamped = stamp.as_deref().map(str::trim) == Some(current.as_str());
    if current == embedded {
        // Ours already (perhaps copied by hand): stamped, so a later build
        // knows it may update it.
        if !stamped {
            write_atomically(&dir.join(INSTALL_STAMP), &embedded)?;
        }
        return Ok(false);
    }
    if present.iter().any(|(_, have)| have.is_some()) && !ours(&current, stamped, &KNOWN_BUILDS) {
        return Err(format!(
            "A changed copy of the DeskLink extension is in {}; it was left as it is",
            dir.display()
        ));
    }
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    for (name, content) in EXTENSION_FILES {
        write_atomically(&dir.join(name), content)?;
    }
    write_atomically(&dir.join(INSTALL_STAMP), &embedded)?;
    Ok(true)
}
